use axum::{
    extract::{Query, State},
    headers::{self, authorization::Basic},
    response::IntoResponse,
    routing::get,
    Router, TypedHeader,
};
use prometheus::{Encoder, Gauge, IntGauge, Opts, TextEncoder};
use reqwest::Client;
use serde::Deserialize;
use std::sync::Arc;

#[derive(Debug, Deserialize)]
struct Config {
    listen: String,
}

struct MetricState {
    client: Arc<Client>,

    ant: Arc<IntGauge>,
    sv: Arc<IntGauge>,

    gps_used: Arc<IntGauge>,
    gps_seen: Arc<IntGauge>,
    bd_used: Arc<IntGauge>,
    bd_seen: Arc<IntGauge>,
    gl_used: Arc<IntGauge>,
    gl_seen: Arc<IntGauge>,

    lat: Arc<Gauge>,
    long: Arc<Gauge>,
    alt: Arc<Gauge>,
}

#[derive(Debug, Deserialize)]
struct Gnss {
    ant: String,
    // r#const: String,
    svused: i64,
    gpsinfo: String,
    bdinfo: String,
    glinfo: String,
    lat: String,
    long: String,
    alt: String,
}

#[derive(Debug, Deserialize)]
struct MetricsQuery {
    target: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = envy::from_env::<Config>()?;
    println!("{config:?}");

    let ant = IntGauge::with_opts(Opts::new("gnss_ant", "Antenna status (0 = OPEN, 1 = OK)"))?;
    let sv = IntGauge::with_opts(Opts::new("gnss_sv", "Satellites used in total"))?;

    let gps_used = IntGauge::with_opts(Opts::new("gnss_gps_used", "GPS satellites used"))?;
    let gps_seen = IntGauge::with_opts(Opts::new("gnss_gps_seen", "GPS satellites seen"))?;
    let bd_used = IntGauge::with_opts(Opts::new("gnss_bd_used", "BeiDou satellites used"))?;
    let bd_seen = IntGauge::with_opts(Opts::new("gnss_bd_seen", "BeiDou satellites seen"))?;
    let gl_used = IntGauge::with_opts(Opts::new("gnss_gl_used", "GLONASS satellites used"))?;
    let gl_seen = IntGauge::with_opts(Opts::new("gnss_gl_seen", "GLONASS satellites seen"))?;

    let lat = Gauge::with_opts(Opts::new("gnss_lat", "Latitude in decimal degrees"))?;
    let long = Gauge::with_opts(Opts::new("gnss_long", "Longitude in decimal degrees"))?;
    let alt = Gauge::with_opts(Opts::new("gnss_alt", "Altitude in meters"))?;

    prometheus::register(Box::new(ant.clone()))?;
    prometheus::register(Box::new(sv.clone()))?;
    prometheus::register(Box::new(gps_used.clone()))?;
    prometheus::register(Box::new(gps_seen.clone()))?;
    prometheus::register(Box::new(bd_used.clone()))?;
    prometheus::register(Box::new(bd_seen.clone()))?;
    prometheus::register(Box::new(gl_used.clone()))?;
    prometheus::register(Box::new(gl_seen.clone()))?;
    prometheus::register(Box::new(lat.clone()))?;
    prometheus::register(Box::new(long.clone()))?;
    prometheus::register(Box::new(alt.clone()))?;

    let metric_state = MetricState {
        client: Arc::new(Client::builder().http1_title_case_headers().build()?),
        ant: Arc::new(ant),
        sv: Arc::new(sv),
        gps_used: Arc::new(gps_used),
        gps_seen: Arc::new(gps_seen),
        bd_used: Arc::new(bd_used),
        bd_seen: Arc::new(bd_seen),
        gl_used: Arc::new(gl_used),
        gl_seen: Arc::new(gl_seen),
        lat: Arc::new(lat),
        long: Arc::new(long),
        alt: Arc::new(alt),
    };

    let app = Router::new()
        .route("/metrics", get(handler))
        .with_state(Arc::new(metric_state));

    axum::Server::bind(&config.listen.parse()?)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

async fn handler(
    Query(query): Query<MetricsQuery>,
    auth: Option<TypedHeader<headers::Authorization<Basic>>>,
    State(metric): State<Arc<MetricState>>,
) -> impl IntoResponse {
    println!("{query:?}");
    // println!("{auth:?}");

    let gnss = if let Some(auth) = auth {
        read_gnss(&query.target, Some((auth.username(), auth.password())), &metric.client).await
    } else {
        read_gnss(&query.target, None, &metric.client).await
    }
    .expect("failed to read GNSS XML");

    update_metrics(&metric, gnss);

    let metrics = prometheus::gather();
    println!("{metrics:?}");
    let mut buffer = Vec::new();

    let encoder = TextEncoder::new();
    encoder.encode(&metrics, &mut buffer).expect("failed to encode metrics");

    buffer
}

async fn read_gnss(target: &str, auth: Option<(&str, &str)>, client: &Client) -> anyhow::Result<Gnss> {
    let mut builder = client.get(target);

    if let Some((username, password)) = auth {
        builder = builder.basic_auth(username, Some(password));
    }

    // let req = builder.build()?;
    // println!("{req:?}");

    // let xml = client.execute(req).await?.text().await?;
    let xml = builder.send().await?.text().await?;
    println!("{xml}");

    let gnss = serde_xml_rs::from_str::<Gnss>(&xml)?;
    println!("{gnss:?}");

    Ok(gnss)
}

fn update_metrics(metric: &MetricState, gnss: Gnss) {
    metric.ant.set(match gnss.ant.as_str() {
        "OPEN" => 0,
        "OK" => 1,
        _ => 2,
    });

    metric.sv.set(gnss.svused);

    if let Ok((gps_used, gps_seen)) = parse_used_seen(&gnss.gpsinfo) {
        metric.gps_used.set(gps_used);
        metric.gps_seen.set(gps_seen);
    }

    if let Ok((bd_used, bd_seen)) = parse_used_seen(&gnss.bdinfo) {
        metric.bd_used.set(bd_used);
        metric.bd_seen.set(bd_seen);
    }

    if let Ok((gl_used, gl_seen)) = parse_used_seen(&gnss.glinfo) {
        metric.gl_used.set(gl_used);
        metric.gl_seen.set(gl_seen);
    }

    if let Ok((lat, long)) = parse_lat_long(&gnss.lat).and_then(|lat| Ok((lat, parse_lat_long(&gnss.long)?))) {
        metric.lat.set(lat);
        metric.long.set(long);
    }

    let alt = gnss.alt.trim_end_matches(" m");
    if let Ok(alt) = alt.parse() {
        metric.alt.set(alt);
    }
}

fn parse_used_seen(v: &str) -> anyhow::Result<(i64, i64)> {
    let (used, seen) = v.split_once('/').ok_or(anyhow::anyhow!("malformed used/seen value"))?;
    let used = used.parse()?;
    let seen = seen.parse()?;

    Ok((used, seen))
}

fn parse_lat_long(v: &str) -> anyhow::Result<f64> {
    let (dir, val) = v.split_once(' ').ok_or(anyhow::anyhow!("malformed lat/long value"))?;
    let sign = match dir {
        "N" | "E" => 1.,
        "S" | "W" => -1.,
        _ => return Err(anyhow::anyhow!("malformed lat/long value")),
    };

    let (dec_min, sec) = val.split_once('.').ok_or(anyhow::anyhow!("malformed lat/long value"))?;

    let (dec, min) = dec_min.split_at(dec_min.len() - 2);
    let dec: f64 = dec.parse()?;
    let min: f64 = min.parse()?;

    let sec = sec.parse::<f64>()? / 1000.0;

    Ok(sign * (dec + min / 60.0 + sec / 3600.0))
}

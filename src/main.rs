use axum::{
    extract::{Query, State},
    headers::{self, authorization::Basic},
    response::IntoResponse,
    routing::get,
    Router, TypedHeader,
};
use prometheus::{Encoder, Gauge, IntGauge, IntGaugeVec, Opts, TextEncoder};
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
    svs_used: Arc<IntGaugeVec>,
    svs_seen: Arc<IntGaugeVec>,

    lat: Arc<Gauge>,
    lon: Arc<Gauge>,
    alt: Arc<Gauge>,
}

#[derive(Debug, Deserialize)]
struct Gnss {
    ant: String,
    // r#const: String,
    // svused: i64,
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
    let svs_used = IntGaugeVec::new(Opts::new("gnss_svs_used", "Satellites used"), &["constellation"])?;
    let svs_seen = IntGaugeVec::new(Opts::new("gnss_svs_seen", "Satellites seen"), &["constellation"])?;

    let lat = Gauge::with_opts(Opts::new("gnss_lat", "Latitude in decimal degrees"))?;
    let lon = Gauge::with_opts(Opts::new("gnss_lon", "Longitude in decimal degrees"))?;
    let alt = Gauge::with_opts(Opts::new("gnss_alt", "Altitude in meters"))?;

    prometheus::register(Box::new(ant.clone()))?;
    prometheus::register(Box::new(svs_seen.clone()))?;
    prometheus::register(Box::new(svs_used.clone()))?;
    prometheus::register(Box::new(lat.clone()))?;
    prometheus::register(Box::new(lon.clone()))?;
    prometheus::register(Box::new(alt.clone()))?;

    let metric_state = MetricState {
        client: Arc::new(Client::builder().http1_title_case_headers().build()?),
        ant: Arc::new(ant),
        svs_used: Arc::new(svs_used),
        svs_seen: Arc::new(svs_seen),
        lat: Arc::new(lat),
        lon: Arc::new(lon),
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

    if let Err(e) = update_metrics(&metric, gnss) {
        println!("failed to update metrics: {e}");
    }

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

fn update_metrics(metric: &MetricState, gnss: Gnss) -> anyhow::Result<()> {
    metric.ant.set(match gnss.ant.as_str() {
        "OPEN" => 0,
        "OK" => 1,
        _ => 2,
    });

    let (gps_used, gps_seen) = parse_used_seen(&gnss.gpsinfo)?;
    let (bd_used, bd_seen) = parse_used_seen(&gnss.bdinfo)?;
    let (gl_used, gl_seen) = parse_used_seen(&gnss.glinfo)?;

    metric.svs_used.with_label_values(&["GPS"]).set(gps_used);
    metric.svs_used.with_label_values(&["BeiDou"]).set(bd_used);
    metric.svs_used.with_label_values(&["GLONASS"]).set(gl_used);

    metric.svs_seen.with_label_values(&["GPS"]).set(gps_seen);
    metric.svs_seen.with_label_values(&["BeiDou"]).set(bd_seen);
    metric.svs_seen.with_label_values(&["GLONASS"]).set(gl_seen);

    let lat = parse_lat_long(&gnss.lat)?;
    let lon = parse_lat_long(&gnss.long)?;

    metric.lat.set(lat);
    metric.lon.set(lon);

    let alt = gnss.alt.trim_end_matches(" m");
    if let Ok(alt) = alt.parse() {
        metric.alt.set(alt);
    }

    Ok(())
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

    let separator = val.find('.').ok_or(anyhow::anyhow!("malformed lat/long value"))?;

    let (degrees, minutes) = val.split_at(separator - 2);
    let degrees: f64 = degrees.parse()?;
    let minutes: f64 = minutes.parse()?;

    let decimal_degrees = minutes / 60.0;
    Ok(sign * (degrees + decimal_degrees))
}

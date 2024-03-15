use rama::{
    http::{
        dep::http::Response,
        layer::{
            compression::CompressionLayer, set_header::SetResponseHeaderLayer, trace::TraceLayer,
        },
        matcher::HttpMatcher,
        server::HttpServer,
        service::web::{k8s_health, WebService},
        Body, HeaderName, HeaderValue, Request, StatusCode,
    },
    rt::Executor,
    service::{
        layer::{limit::policy::ConcurrentPolicy, HijackLayer, LimitLayer, TimeoutLayer},
        service_fn,
        util::backoff::ExponentialBackoff,
        ServiceBuilder,
    },
    tcp::server::TcpListener,
};
use std::{convert::Infallible, time::Duration};
use tracing::level_filters::LevelFilter;
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod data;
mod endpoints;
mod report;
mod state;

pub use state::State;

pub async fn run(interface: String, port: u16, health_port: u16) -> anyhow::Result<()> {
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::builder()
                .with_default_directive(LevelFilter::INFO.into())
                .from_env_lossy(),
        )
        .init();

    let graceful = rama::graceful::Shutdown::default();

    let health_address = format!("{}:{}", interface, health_port);

    graceful.spawn_task_fn(|guard| async move {
        let exec = Executor::graceful(guard.clone());

        tracing::info!("FP Health Service listening on: {health_address}");

        HttpServer::auto(exec)
            .listen(health_address, k8s_health())
            .await
            .unwrap();
    });

    let http_address = format!("{}:{}", interface, port);

    graceful.spawn_task_fn(|guard| async move {
        tracing::info!("FP Service listening on: {http_address}");
        TcpListener::build_with_state(State::default())
            .bind(http_address)
            .await
            .expect("bind TCP Listener")
            .serve_graceful(
                guard.clone(),
                ServiceBuilder::new()
                    .map_result(|result| {
                        if let Err(err) = result {
                            tracing::warn!(error = %err, "rama service failed");
                        }
                        Ok::<_, Infallible>(())
                    })
                    .layer(TimeoutLayer::new(Duration::from_secs(8)))
                    .layer(LimitLayer::new(ConcurrentPolicy::with_backoff(
                        2048,
                        ExponentialBackoff::default(),
                    )))
                    .service(
                        HttpServer::auto(Executor::graceful(guard)).service(
                            ServiceBuilder::new()
                                .layer(TraceLayer::new_for_http())
                                .layer(CompressionLayer::new())
                                .layer(SetResponseHeaderLayer::appending(
                                    HeaderName::from_static("set-cookie"),
                                    HeaderValue::from_static("rama-fp-version=0.2; Max-Age=60"),
                                ))
                                .layer(HijackLayer::new(
                                    HttpMatcher::header_exists(HeaderName::from_static("cookie"))
                                        .negate(),
                                    service_fn(|req: Request| async move {
                                        Ok::<_, Infallible>(
                                            Response::builder()
                                                .status(StatusCode::TEMPORARY_REDIRECT)
                                                .header("location", req.uri().to_string())
                                                .body(Body::empty())
                                                .expect("build redirect response"),
                                        )
                                    }),
                                ))
                                .service(
                                    WebService::default()
                                        // Navigate
                                        .get("/", endpoints::get_root)
                                        // XHR
                                        .get("/api/fetch/number", endpoints::get_api_fetch_number)
                                        .post(
                                            "/api/fetch/number/:number",
                                            endpoints::post_api_fetch_number,
                                        )
                                        .get(
                                            "/api/xml/number",
                                            endpoints::get_api_xml_http_request_number,
                                        )
                                        .post(
                                            "/api/xml/number/:number",
                                            endpoints::post_api_xml_http_request_number,
                                        )
                                        // Form
                                        .get("/form", endpoints::form)
                                        .post("/form", endpoints::form)
                                        // Assets
                                        .get("/assets/style.css", endpoints::get_assets_style)
                                        .get("/assets/script.js", endpoints::get_assets_script),
                                ),
                        ),
                    ),
            )
            .await;
    });

    graceful
        .shutdown_with_limit(Duration::from_secs(30))
        .await?;

    Ok(())
}

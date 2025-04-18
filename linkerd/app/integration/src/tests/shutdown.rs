use linkerd_app_core::svc::http::BoxBody;

use crate::*;

#[tokio::test]
async fn h2_goaways_connections() {
    let _trace = trace_init();

    let (shdn, rx) = shutdown_signal();

    let srv = server::http2().route("/", "hello").run().await;
    let proxy = proxy::new().inbound(srv).shutdown_signal(rx).run().await;
    let client = client::http2(proxy.inbound, "shutdown.example.com");

    assert_eq!(client.get("/").await, "hello");

    shdn.signal();

    client.wait_for_closed().await;
}

#[tokio::test]
async fn h2_exercise_goaways_connections() {
    let _trace = trace_init();

    const RESPONSE_SIZE: usize = 1024 * 16;
    const NUM_REQUESTS: usize = 50;

    let (shdn, rx) = shutdown_signal();

    let body = http_body_util::Full::new(Bytes::from(vec![b'1'; RESPONSE_SIZE]));
    let srv = server::http2()
        .route_fn("/", move |_req| {
            Response::builder()
                .body(body.clone())
                .unwrap()
                .map(BoxBody::new)
        })
        .run()
        .await;
    let proxy = proxy::new().inbound(srv).shutdown_signal(rx).run().await;
    let client = client::http2(proxy.inbound, "shutdown.example.com");

    let reqs = (0..NUM_REQUESTS)
        .map(|_| client.request(client.request_builder("/").method("GET")))
        .collect::<Vec<_>>();

    // Wait to get all responses (but not bodies)
    let resps = future::try_join_all(reqs).await.expect("reqs");

    // Trigger a shutdown while bodies are still in progress.
    shdn.signal();

    let bodies = resps
        .into_iter()
        .map(Response::into_body)
        .map(|body| {
            http_body_util::BodyExt::collect(body)
                .map_ok(http_body_util::Collected::aggregate)
                // Make sure the bodies weren't cut off
                .map_ok(|buf| assert_eq!(buf.remaining(), RESPONSE_SIZE))
        })
        .collect::<Vec<_>>();

    // See that the proxy gives us all the bodies.
    future::try_join_all(bodies).await.expect("bodies");

    client.wait_for_closed().await;
}

#[tokio::test]
async fn http1_closes_idle_connections() {
    use parking_lot::Mutex;
    use std::sync::Arc;
    let _trace = trace_init();

    let (shdn, rx) = shutdown_signal();

    const RESPONSE_SIZE: usize = 1024 * 16;
    let body = http_body_util::Full::new(Bytes::from(vec![b'1'; RESPONSE_SIZE]));

    let shdn = Arc::new(Mutex::new(Some(shdn)));
    let srv = server::http1()
        .route_fn("/", move |_req| {
            // Trigger a shutdown signal while the request is made
            // but a response isn't returned yet.
            shdn.lock().take().expect("only 1 request").signal();
            Response::builder()
                .body(body.clone())
                .unwrap()
                .map(BoxBody::new)
        })
        .run()
        .await;
    let proxy = proxy::new().inbound(srv).shutdown_signal(rx).run().await;
    let client = client::http1(proxy.inbound, "shutdown.example.com");

    let res_body = client.get("/").await;
    assert_eq!(res_body.len(), RESPONSE_SIZE);

    client.wait_for_closed().await;
}

#[tokio::test]
async fn tcp_waits_for_proxies_to_close() {
    let _trace = trace_init();

    let (shdn, rx) = shutdown_signal();
    let msg1 = "custom tcp hello\n";
    let msg2 = "custom tcp bye";

    let srv = crate::tcp::server()
        // Trigger a shutdown while TCP stream is busy
        .accept_fut(move |mut sock| {
            async move {
                shdn.signal();
                let mut vec = vec![0; 256];
                let n = sock.read(&mut vec).await?;
                assert_eq!(&vec[..n], msg1.as_bytes());
                sock.write_all(msg2.as_bytes()).await
            }
            .map(|res| res.expect("TCP server must not error"))
        })
        .run()
        .await;
    let proxy = proxy::new().inbound(srv).shutdown_signal(rx).run().await;

    let client = crate::tcp::client(proxy.inbound);

    let tcp_client = client.connect().await;

    tcp_client.write(msg1).await;
    assert_eq!(tcp_client.read().await, msg2.as_bytes());
    tcp_client.shutdown().await;
    proxy.join_servers().await; // propagate panics from assertions in the server
}

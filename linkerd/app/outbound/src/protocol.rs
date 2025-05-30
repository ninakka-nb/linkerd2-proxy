use crate::{http, Outbound, ParentRef};
use linkerd_app_core::{io, svc, Error, Infallible};
use std::{fmt::Debug, hash::Hash};

mod metrics;
#[cfg(test)]
mod tests;

pub use self::metrics::MetricsFamilies;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Http<T> {
    version: http::Variant,
    parent: T,
}

/// Parameter type indicating how the proxy should handle a connection.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum Protocol {
    Http1,
    Http2,
    Detect,
    Opaque,
    Tls,
}

// === impl Outbound ===

impl<N> Outbound<N> {
    /// Builds a stack that handles protocol detection as well as routing and
    /// load balancing for a single logical destination.
    ///
    /// The inner stack is built once for each `T` target when the protocol is
    /// known. When `Protocol::Detect` is used, the inner stack is built for
    /// each connection.
    pub fn push_protocol<T, I, NSvc>(
        self,
        http: svc::ArcNewCloneHttp<Http<T>>,
        tls: svc::ArcNewCloneTcp<T, io::EitherIo<I, io::PrefixedIo<I>>>,
    ) -> Outbound<svc::ArcNewTcp<T, I>>
    where
        // Target type indicating whether detection should be skipped.
        T: svc::Param<Protocol>,
        T: svc::Param<ParentRef>,
        T: Eq + Hash + Clone + Debug + Send + Sync + 'static,
        // Server-side socket.
        I: io::AsyncRead + io::AsyncWrite + io::PeerAddr,
        I: Debug + Send + Sync + Unpin + 'static,
        // Opaque connection stack.
        N: svc::NewService<T, Service = NSvc>,
        N: Clone + Send + Sync + Unpin + 'static,
        NSvc: svc::Service<io::EitherIo<I, io::PrefixedIo<I>>, Response = (), Error = Error>,
        NSvc: Clone + Send + Sync + Unpin + 'static,
        NSvc::Future: Send,
    {
        let opaq = self.clone().into_stack();

        let http = self.with_stack(http).map_stack(|config, rt, stk| {
            let h2 = config.proxy.server.http2.clone();
            let drain = rt.drain.clone();
            stk.unlift_new()
                .push(http::NewServeHttp::layer(move |t: &Http<T>| {
                    http::ServerParams {
                        version: t.version,
                        http2: h2.clone(),
                        drain: drain.clone(),
                    }
                }))
                .arc_new_tcp()
        });

        let detect = http.clone().map_stack(|config, rt, http| {
            let read_timeout = config.proxy.detect_protocol_timeout;
            let metrics = rt.metrics.prom.http_detect.clone();

            http.push_switch(
                |(detected, parent): (http::Detection, T)| -> Result<_, Infallible> {
                    match detected {
                        http::Detection::Http(version) => {
                            return Ok(svc::Either::Left(Http { version, parent }));
                        }
                        http::Detection::ReadTimeout(timeout) => {
                            tracing::info!("Continuing after timeout: {timeout:?}");
                        }
                        _ => {}
                    }
                    Ok(svc::Either::Right(parent))
                },
                opaq.clone().into_inner(),
            )
            // `DetectService` oneshots the inner service, so we add
            // a loadshed to prevent leaking tasks if (for some
            // unexpected reason) the inner service is not ready.
            .push_on_service(svc::LoadShed::layer())
            .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Right))
            .lift_new_with_target::<(http::Detection, T)>()
            .push(http::NewDetect::layer(move |parent: &T| {
                http::DetectParams {
                    read_timeout,
                    metrics: metrics.metrics(parent.param()),
                }
            }))
            .arc_new_tcp()
        });

        http.map_stack(|_, rt, http| {
            // First separate traffic that needs protocol detection. Then switch
            // between traffic that is known to be HTTP or opaque.
            let known = http.push_switch(
                Ok::<_, Infallible>,
                opaq.clone()
                    .push_switch(Ok::<_, Infallible>, tls.clone())
                    .into_inner(),
            );

            known
                .push_on_service(svc::MapTargetLayer::new(io::EitherIo::Left))
                .push_switch(
                    |parent: T| -> Result<_, Infallible> {
                        match parent.param() {
                            Protocol::Http1 => Ok(svc::Either::Left(svc::Either::Left(Http {
                                version: http::Variant::Http1,
                                parent,
                            }))),
                            Protocol::Http2 => Ok(svc::Either::Left(svc::Either::Left(Http {
                                version: http::Variant::H2,
                                parent,
                            }))),
                            Protocol::Opaque => Ok(svc::Either::Left(svc::Either::Right(
                                svc::Either::Left(parent),
                            ))),
                            Protocol::Tls => Ok(svc::Either::Left(svc::Either::Right(
                                svc::Either::Right(parent),
                            ))),
                            Protocol::Detect => Ok(svc::Either::Right(parent)),
                        }
                    },
                    detect.into_inner(),
                )
                .push(metrics::NewRecord::layer(rt.metrics.prom.protocol.clone()))
                .arc_new_tcp()
        })
    }
}

// === impl Http ===

impl<T> From<(http::Variant, T)> for Http<T> {
    fn from((version, parent): (http::Variant, T)) -> Self {
        Self { version, parent }
    }
}

impl<T> svc::Param<http::Variant> for Http<T> {
    fn param(&self) -> http::Variant {
        self.version
    }
}

impl<T> svc::Param<http::normalize_uri::DefaultAuthority> for Http<T>
where
    T: svc::Param<http::normalize_uri::DefaultAuthority>,
{
    fn param(&self) -> http::normalize_uri::DefaultAuthority {
        self.parent.param()
    }
}

impl<T> std::ops::Deref for Http<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.parent
    }
}

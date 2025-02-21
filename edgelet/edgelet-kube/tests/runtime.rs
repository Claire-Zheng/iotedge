// Copyright (c) Microsoft. All rights reserved.

#![deny(rust_2018_idioms, warnings)]
#![deny(clippy::all, clippy::pedantic)]

use std::path::Path;

use config::{Config, File, FileFormat};
use futures::future::FutureResult;
use futures::{future, Future};
use hyper::client::{Client as HyperClient, HttpConnector};
use hyper::{header, Body, Method, Request, Response, StatusCode};
use json_patch::merge;
use maplit::btreemap;
use native_tls::TlsConnector;
use serde_json::{self, json, Value as JsonValue};
use tokio::runtime::Runtime;
use typed_headers::{mime, ContentLength, ContentType, HeaderMapExt};
use url::Url;

use edgelet_core::{
    AuthId, Authenticator, Certificates, Connect, GetTrustBundle, Listen, MakeModuleRuntime,
    ModuleSpec, Provisioning, ProvisioningResult as CoreProvisioningResult, RuntimeSettings,
    WatchdogSettings,
};
use edgelet_docker::DockerConfig;
use edgelet_kube::{ErrorKind, KubeModuleRuntime, Settings};
use edgelet_test_utils::crypto::TestHsm;
use edgelet_test_utils::web::{
    make_req_dispatcher, HttpMethod, RequestHandler, RequestPath, ResponseFuture,
};
use edgelet_test_utils::{get_unused_tcp_port, routes, run_tcp_server};
use kube_client::{Client as KubeClient, Config as KubeConfig, Error, HttpClient, TokenSource};
use provisioning::{ProvisioningResult, ReprovisioningStatus};

fn not_found_handler(_: Request<Body>) -> ResponseFuture {
    let response = Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::default())
        .unwrap();

    Box::new(future::ok(response))
}

fn make_settings(merge_json: Option<JsonValue>) -> Settings {
    let mut config = Config::default();
    let mut config_json = json!({
        "provisioning": {
            "source": "manual",
            "device_connection_string": "HostName=moo.azure-devices.net;DeviceId=boo;SharedAccessKey=boo"
        },
        "agent": {
            "name": "edgeAgent",
            "type": "docker",
            "env": {},
            "config": {
                "image": "mcr.microsoft.com/azureiotedge-agent:1.0",
                "auth": {}
            }
        },
        "hostname": "default1",
        "connect": {
            "management_uri": "http://localhost:35000",
            "workload_uri": "http://localhost:35001"
        },
        "listen": {
            "management_uri": "http://localhost:35000",
            "workload_uri": "http://localhost:35001"
        },
        "homedir": "/var/lib/iotedge",
        "namespace": "default",
        "use_pvc": true,
        "iot_hub_hostname": "iotHub",
        "device_id": "device1",
        "proxy_image": "proxy:latest",
        "proxy_config_path": "/etc/traefik",
        "proxy_config_map_name": "device1-iotedged-proxy-config",
        "proxy_trust_bundle_path": "/etc/trust-bundle",
        "proxy_trust_bundle_config_map_name": "device1-iotedged-proxy-trust-bundle",
        "image_pull_policy": "IfNotPresent",
        "service_account_name": "iotedge",
        "device_hub_selector": "",
    });

    if let Some(merge_json) = merge_json {
        merge(&mut config_json, &merge_json);
    }

    config
        .merge(File::from_str(&config_json.to_string(), FileFormat::Json))
        .unwrap();

    config.try_into().unwrap()
}

#[test]
fn authenticate_returns_none_when_no_auth_token_provided() {
    let port = get_unused_tcp_port();

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => unauthenticated_token_review_handler()
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let (_, runtime) = create_runtime(&format!("http://localhost:{}", port));

    let req = Request::default();

    let task = runtime.authenticate(&req);

    let mut runtime = Runtime::new().unwrap();
    runtime.spawn(server);
    let auth_id = runtime.block_on(task).unwrap();

    assert_eq!(auth_id, AuthId::None)
}

#[test]
fn authenticate_returns_none_when_invalid_auth_header_provided() {
    let port = get_unused_tcp_port();

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => unauthenticated_token_review_handler()
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let (_, runtime) = create_runtime(&format!("http://localhost:{}", port));

    let mut req = Request::default();
    req.headers_mut()
        .insert(header::AUTHORIZATION, "BeErer token".parse().unwrap());

    let task = runtime.authenticate(&req);

    let mut runtime = Runtime::new().unwrap();
    runtime.spawn(server);
    let auth_id = runtime.block_on(task).unwrap();

    assert_eq!(auth_id, AuthId::None)
}

#[test]
fn authenticate_returns_none_when_invalid_auth_token_provided() {
    let port = get_unused_tcp_port();

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => unauthenticated_token_review_handler()
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let (_, runtime) = create_runtime(&format!("http://localhost:{}", port));

    let mut req = Request::default();
    req.headers_mut().insert(
        header::AUTHORIZATION,
        "\u{3aa}\u{3a9}\u{3a4}".parse().unwrap(),
    );

    let task = runtime.authenticate(&req);

    let mut runtime = Runtime::new().unwrap();
    runtime.spawn(server);
    let err = runtime.block_on(task).unwrap_err();

    assert_eq!(err.kind(), &ErrorKind::ModuleAuthenticationError);
}

#[test]
fn authenticate_returns_none_when_unknown_auth_token_provided() {
    let port = get_unused_tcp_port();

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => unauthenticated_token_review_handler()
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let (_, runtime) = create_runtime(&format!("http://localhost:{}", port));

    let mut req = Request::default();
    req.headers_mut().insert(
        header::AUTHORIZATION,
        "Bearer token-unknown".parse().unwrap(),
    );

    let task = runtime.authenticate(&req);

    let mut runtime = Runtime::new().unwrap();
    runtime.spawn(server);
    let auth_id = runtime.block_on(task).unwrap();

    assert_eq!(auth_id, AuthId::None)
}

#[test]
fn authenticate_returns_none_when_module_auth_token_provided_but_service_account_does_not_exists() {
    let port = get_unused_tcp_port();

    let (_, runtime) = create_runtime(&format!("http://localhost:{}", port));

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => authenticated_token_review_handler(),
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let mut req = Request::default();
    req.headers_mut()
        .insert(header::AUTHORIZATION, "Bearer token".parse().unwrap());

    let task = runtime.authenticate(&req);

    let mut runtime = Runtime::new().unwrap();
    runtime.spawn(server);
    let err = runtime.block_on(task).unwrap_err();

    assert_eq!(err.kind(), &ErrorKind::KubeClient);
}

#[test]
fn authenticate_returns_sa_name_when_module_auth_token_provided_but_service_account_does_not_contain_original_name(
) {
    let port = get_unused_tcp_port();

    let (settings, runtime) = create_runtime(&format!("http://localhost:{}", port));

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => authenticated_token_review_handler(),
        GET format!("/api/v1/namespaces/{}/serviceaccounts/edgeagent", settings.namespace()) => get_service_account_without_annotations_handler(),
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let mut req = Request::default();
    req.headers_mut()
        .insert(header::AUTHORIZATION, "Bearer token".parse().unwrap());

    let task = runtime.authenticate(&req);

    let mut runtime = Runtime::new().unwrap();
    runtime.spawn(server);
    let auth_id = runtime.block_on(task).unwrap();

    assert_eq!(auth_id, AuthId::Value("edgeagent".into()));
}

#[test]
fn authenticate_returns_auth_id_when_module_auth_token_provided() {
    let port = get_unused_tcp_port();

    let (settings, runtime) = create_runtime(&format!("http://localhost:{}", port));

    let dispatch_table = routes!(
        POST "/apis/authentication.k8s.io/v1/tokenreviews" => authenticated_token_review_handler(),
        GET format!("/api/v1/namespaces/{}/serviceaccounts/edgeagent", settings.namespace()) => get_service_account_with_annotations_handler(),
    );

    let server = run_tcp_server(
        "127.0.0.1",
        port,
        make_req_dispatcher(dispatch_table, Box::new(not_found_handler)),
    )
    .map_err(|err| eprintln!("{}", err));

    let mut req = Request::default();
    req.headers_mut()
        .insert(header::AUTHORIZATION, "Bearer token".parse().unwrap());

    let task = runtime.authenticate(&req);

    let mut runtime = Runtime::new().unwrap();
    runtime.spawn(server);
    let auth_id = runtime.block_on(task).unwrap();

    assert_eq!(auth_id, AuthId::Value("$edgeAgent".into()));
}

#[derive(Clone)]
struct TestKubeSettings {
    kube_settings: Settings,
    api_server: Url,
}

impl TestKubeSettings {
    fn new(kube_settings: Settings, api_server: Url) -> Self {
        Self {
            kube_settings,
            api_server,
        }
    }

    fn into_kube_settings(self) -> Settings {
        self.kube_settings
    }

    fn api_server(&self) -> &Url {
        &self.api_server
    }

    fn with_device_id(mut self, device_id: &str) -> Self {
        self.kube_settings = self.kube_settings.with_device_id(device_id);
        self
    }

    fn with_iot_hub_hostname(mut self, iot_hub_hostname: &str) -> Self {
        self.kube_settings = self.kube_settings.with_iot_hub_hostname(iot_hub_hostname);
        self
    }

    fn namespace(&self) -> &str {
        self.kube_settings.namespace()
    }
}

impl RuntimeSettings for TestKubeSettings {
    type Config = DockerConfig;

    fn provisioning(&self) -> &Provisioning {
        self.kube_settings.provisioning()
    }

    fn agent(&self) -> &ModuleSpec<Self::Config> {
        self.kube_settings.agent()
    }

    fn agent_mut(&mut self) -> &mut ModuleSpec<Self::Config> {
        self.kube_settings.agent_mut()
    }

    fn hostname(&self) -> &str {
        self.kube_settings.hostname()
    }

    fn connect(&self) -> &Connect {
        self.kube_settings.connect()
    }

    fn listen(&self) -> &Listen {
        self.kube_settings.listen()
    }

    fn homedir(&self) -> &Path {
        self.kube_settings.homedir()
    }

    fn certificates(&self) -> Option<&Certificates> {
        self.kube_settings.certificates()
    }

    fn watchdog(&self) -> &WatchdogSettings {
        self.kube_settings.watchdog()
    }
}

struct TestKubeModuleRuntime(KubeModuleRuntime<TestTokenSource, HttpClient<HttpConnector, Body>>);

impl MakeModuleRuntime for TestKubeModuleRuntime {
    type Config = DockerConfig;
    type Settings = TestKubeSettings;
    type ProvisioningResult = ProvisioningResult;
    type ModuleRuntime = KubeModuleRuntime<TestTokenSource, HttpClient<HttpConnector, Body>>;
    type Error = Error;
    type Future = FutureResult<Self::ModuleRuntime, Self::Error>;

    fn make_runtime(
        settings: Self::Settings,
        provisioning_result: Self::ProvisioningResult,
        _: impl GetTrustBundle,
    ) -> Self::Future {
        let settings = settings
            .with_device_id(provisioning_result.device_id())
            .with_iot_hub_hostname(provisioning_result.hub_name());

        future::ok(KubeModuleRuntime::new(
            KubeClient::with_client(
                get_config(settings.api_server()),
                HttpClient(HyperClient::new()),
            ),
            settings.into_kube_settings(),
        ))
    }
}

fn create_runtime(
    url: &str,
) -> (
    TestKubeSettings,
    KubeModuleRuntime<TestTokenSource, HttpClient<HttpConnector, Body>>,
) {
    let provisioning_result = ProvisioningResult::new(
        "my_device_id",
        "iothostname",
        None,
        ReprovisioningStatus::DeviceDataNotUpdated,
        None,
    );
    let settings = TestKubeSettings::new(make_settings(None), url.parse().unwrap());
    let runtime = TestKubeModuleRuntime::make_runtime(
        settings.clone(),
        provisioning_result,
        TestHsm::default(),
    )
    .wait()
    .unwrap();

    (settings, runtime)
}

fn get_config(api_server: &Url) -> KubeConfig<TestTokenSource> {
    KubeConfig::new(
        api_server.clone(),
        "/api".to_string(),
        TestTokenSource,
        TlsConnector::new().unwrap(),
    )
}

fn authenticated_token_review_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    make_token_review_handler(|| {
        json!({
            "kind": "TokenReview",
            "spec": { "token": "token" },
            "status": {
                "authenticated": true,
                "user": {
                    "username": "system:serviceaccount:my-namespace:edgeagent"
                }
            }}
        )
        .to_string()
    })
}

fn unauthenticated_token_review_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    make_token_review_handler(|| {
        json!({
            "kind": "TokenReview",
            "spec": { "token": "token" },
            "status": {
                "authenticated": false,
            }}
        )
        .to_string()
    })
}

fn make_token_review_handler(
    on_token_review: impl Fn() -> String + Clone + Send + 'static,
) -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |_| {
        let response = on_token_review();
        let response_len = response.len();

        let mut response = Response::new(response.into());
        response
            .headers_mut()
            .typed_insert(&ContentLength(response_len as u64));
        response
            .headers_mut()
            .typed_insert(&ContentType(mime::APPLICATION_JSON));
        Box::new(future::ok(response)) as ResponseFuture
    }
}

fn get_service_account_with_annotations_handler() -> impl Fn(Request<Body>) -> ResponseFuture + Clone
{
    move |_| {
        response(StatusCode::OK, || {
            json!({
                "kind": "ServiceAccount",
                "apiVersion": "v1",
                "metadata": {
                    "name": "edgeagent",
                    "namespace": "my-namespace",
                    "annotations": {
                        "net.azure-devices.edge.original-moduleid": "$edgeAgent"
                    }
                }
            })
            .to_string()
        })
    }
}

fn get_service_account_without_annotations_handler(
) -> impl Fn(Request<Body>) -> ResponseFuture + Clone {
    move |_| {
        response(StatusCode::OK, || {
            json!({
                "kind": "ServiceAccount",
                "apiVersion": "v1",
                "metadata": {
                    "name": "edgeagent",
                    "namespace": "my-namespace",
                }
            })
            .to_string()
        })
    }
}

fn response(
    status_code: StatusCode,
    response: impl Fn() -> String + Clone + Send + 'static,
) -> ResponseFuture {
    let response = response();
    let response_len = response.len();

    let mut response = Response::new(response.into());
    *response.status_mut() = status_code;
    response
        .headers_mut()
        .typed_insert(&ContentLength(response_len as u64));
    response
        .headers_mut()
        .typed_insert(&ContentType(mime::APPLICATION_JSON));

    Box::new(future::ok(response)) as ResponseFuture
}

#[derive(Clone)]
struct TestTokenSource;

impl TokenSource for TestTokenSource {
    type Error = Error;

    fn get(&self) -> kube_client::error::Result<Option<String>> {
        Ok(None)
    }
}

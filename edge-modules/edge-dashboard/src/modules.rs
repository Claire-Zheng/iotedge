// Copyright (c) Microsoft. All rights reserved.

use std::sync::Arc;

use actix_web::error::ErrorInternalServerError;
use actix_web::Error as ActixError;
use actix_web::*;
use edgelet_core::{LogOptions, Module as EdgeModule, ModuleRuntime, RuntimeSettings};
use edgelet_http_mgmt::*;
use futures::future::{ok, Either, IntoFuture};
use futures::stream::Stream;
use futures::{Async, Future};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use url::Url;

use crate::health::HealthStatus;
use crate::health::Status;
use crate::AuthRequest;
use crate::Context;

#[derive(Debug, Deserialize, Serialize)]
pub struct Module {
    id: String,
    r#type: String,
    status: String,
    config: TConfig,
    cpu: i32,
    memoryInMb: i32,
}

impl Module {
    pub fn new(id: String, status: String) -> Self {
        Module {
            id: id.clone(),
            r#type: String::from("docker"),
            status,
            config: TConfig::new(id.clone()),
            cpu: *vec![0, 20, 14, 33, 27, 24, 4, 8].choose(&mut rand::thread_rng()).unwrap(),
            memoryInMb: *vec![150, 200, 140, 175, 190, 80, 125, 75].choose(&mut rand::thread_rng()).unwrap(),
        }
    }

    pub fn id(&self) -> &String {
        &self.id
    }

    pub fn status(&self) -> &String {
        &self.status
    }
}

#[derive(Debug, Deserialize, Serialize)]
pub struct TConfig {
    image: String,
}

impl TConfig {
    pub fn new(id: String) -> Self {
        TConfig {
            image: format!("mcr.microsoft.com/{}:1.0", id),
        }
    }
}

pub fn restart_module(
    req: HttpRequest,
    context: web::Data<Arc<Context>>,
    info: web::Query<AuthRequest>,
) -> Box<dyn Future<Item = HttpResponse, Error = ActixError>> {
    let api_ver = &info.api_version;
    let response = req
        .match_info()
        .get("id")
        .map(|module_id| {
            context
                .edge_config
                .as_ref()
                .map(|config| {
                    let mgmt_uri = config.connect().management_uri();
                    Either::A(
                        Url::parse(&format!("{}/modules/?api-version={}", mgmt_uri, api_ver))
                            .map_err(ErrorInternalServerError)
                            .and_then(|url| {
                                ModuleClient::new(&url).map_err(ErrorInternalServerError)
                            })
                            .map(|mod_client| {
                                mod_client
                                    .restart(module_id)
                                    .map_err(ErrorInternalServerError)
                                    .map(|_| {
                                        HttpResponse::Ok().body(format!("Module has restarted"))
                                    })
                            })
                            .into_future()
                            .flatten(),
                    )
                })
                .unwrap_or_else(|err| {
                    Either::B(ok(HttpResponse::ServiceUnavailable()
                        .content_type("text/plain")
                        .body(format!("{:?}", err))))
                })
        })
        .unwrap_or_else(|| Either::B(ok(HttpResponse::BadRequest().body("Invalid module ID"))));

    Box::new(response)
}

pub fn get_logs(
    req: HttpRequest,
    context: web::Data<Arc<Context>>,
    info: web::Query<AuthRequest>,
) -> Box<dyn Future<Item = HttpResponse, Error = ActixError>> {
    let api_ver = &info.api_version;

    let response = req
        .match_info()
        .get("id")
        .map(|module_id| {
            context
                .edge_config
                .as_ref()
                .map(move |config| {
                    let mgmt_uri = config.connect().management_uri();
                    Either::A(
                        Url::parse(&format!("{}/modules/?api-version={}", mgmt_uri, api_ver))
                            .map_err(ErrorInternalServerError)
                            .and_then(|url| {
                                ModuleClient::new(&url).map_err(ErrorInternalServerError)
                            }) // can't connect to the endpoint
                            .map(move |mod_client| {
                                mod_client
                                    .logs(module_id, &LogOptions::new())
                                    .map_err(ErrorInternalServerError)
                                    .and_then(|data| {
                                        data.map_err(ErrorInternalServerError)
                                            .fold(Vec::new(), |mut acc, chunk| {
                                                let stream = chunk.as_ref();
                                                if stream.len() >= 8 {
                                                    let (_, right) = stream.split_at(8);
                                                    acc.extend_from_slice(right);
                                                }
                                                Ok::<_, ActixError>(acc)
                                            })
                                            .and_then(|body| {
                                                let mut clone = body.clone();
                                                clone.retain(|&byte| (byte as char).is_ascii());
                                                if let Ok(content) = String::from_utf8(clone) {
                                                    HttpResponse::Ok().body(content)
                                                } else {
                                                    HttpResponse::ServiceUnavailable()
                                                        .body("Logs unable to be displayed")
                                                }
                                            })
                                    })
                            })
                            .into_future()
                            .flatten(),
                    )
                })
                .unwrap_or_else(|err| {
                    Either::B(ok(HttpResponse::ServiceUnavailable()
                        .content_type("text/plain")
                        .body(format!("{:?}", err))))
                })
        })
        .unwrap_or_else(|| Either::B(ok(HttpResponse::BadRequest().body("Invalid module ID"))));

    Box::new(response)
}

pub fn get_modules(
    context: web::Data<Arc<Context>>,
    info: web::Query<AuthRequest>,
) -> Box<dyn Future<Item = HttpResponse, Error = ActixError>> {
    return_modules(context, &info.api_version, module_response)
}

fn module_response(mods: Vec<Module>) -> HttpResponse {
    serde_json::to_string(&mods)
        .map(|json| {
            HttpResponse::Ok()
                .content_type("text/json")
                .body(format!("{}", json))
        })
        .unwrap_or(HttpResponse::ServiceUnavailable().body("Unable to convert output to JSON"))
}

pub fn get_health(
    context: web::Data<Arc<Context>>,
    info: web::Query<AuthRequest>,
) -> Box<dyn Future<Item = HttpResponse, Error = ActixError>> {
    return_modules(context, &info.api_version, health_response)
}

fn health_response(mods: Vec<Module>) -> HttpResponse {
    let mut device_status = Status::new();
    let edge_agent = mods
        .iter()
        .any(|module| module.id() == "edgeAgent" && module.status() == "running");

    let edge_hub = mods
        .iter()
        .any(|module| module.id() == "edgeHub" && module.status() == "running");

    let other = mods.iter().any(|module| module.status() != "running");

    device_status.set_iotedged();
    device_status.set_edge_agent(edge_agent);
    device_status.set_edge_hub(edge_hub);
    device_status.set_other_modules(edge_agent && edge_hub && other);

    let health = device_status.return_health();

    serde_json::to_string(&HealthStatus::new(health, device_status))
        .map(|json| {
            HttpResponse::Ok()
                .content_type("text/json")
                .body(json)
        })
        .unwrap_or(HttpResponse::ServiceUnavailable().body("Unable to convert to JSON"))
}

fn return_modules(
    context: web::Data<Arc<Context>>,
    api_ver: &str,
    f: fn(Vec<Module>) -> HttpResponse,
) -> Box<dyn Future<Item = HttpResponse, Error = ActixError>> {
    let response = context
        .edge_config
        .as_ref()
        .map(move |config| {
            let mgmt_uri = config.connect().management_uri();
            Either::A(
                Url::parse(&format!("{}/modules/?api-version={}", mgmt_uri, api_ver))
                    .map_err(ErrorInternalServerError)
                    .and_then(|url| ModuleClient::new(&url).map_err(ErrorInternalServerError))
                    .map(|mod_client| {
                        mod_client
                            .list()
                            .map(move |data| {
                                let mods: Vec<Module> = data
                                    .iter()
                                    .map(move |c| {
                                        let status =
                                            if let Ok(Async::Ready(t)) = c.runtime_state().poll() {
                                                (*(t.status().clone()).to_string()).to_string()
                                            } else {
                                                "".to_string()
                                            };
                                        Module::new(c.name().to_string(), status)
                                    })
                                    .collect();
                                f(mods) // changes depending on API call
                            })
                            .map_err(ErrorInternalServerError)
                    })
                    .into_future()
                    .flatten(),
            )
        })
        .unwrap_or_else(|err| {
            Either::B(ok(HttpResponse::ServiceUnavailable()
                .content_type("text/plain")
                .body(format!("{:?}", err))))
        });

    Box::new(response)
}

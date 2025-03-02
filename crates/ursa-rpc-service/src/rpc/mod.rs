use std::sync::Arc;

use anyhow::Result;
use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Extension, Json,
};
use jsonrpc_v2::{Data, Error, MapRouter, RequestObject, ResponseObject, ResponseObjects, Server};

use self::routes::network;
use crate::api::NetworkInterface;

pub mod routes;

#[derive(Clone)]
pub struct RpcServer(Arc<Server<MapRouter>>);

pub enum ServerErrors {
    ApiError(Error),
}
impl IntoResponse for ServerErrors {
    fn into_response(self) -> Response {
        let body = match self {
            ServerErrors::ApiError(e) => Json(e),
        };
        (StatusCode::INTERNAL_SERVER_ERROR, body).into_response()
    }
}

pub async fn rpc_handler(
    Extension(server): Extension<RpcServer>,
    Json(req): Json<RequestObject>,
) -> Result<Json<ResponseObjects>, ServerErrors> {
    match server.0.handle(req).await {
        ResponseObjects::One(r) => match r {
            ResponseObject::Result {
                jsonrpc,
                result,
                id,
            } => Ok(Json(ResponseObjects::One(ResponseObject::Result {
                jsonrpc,
                result,
                id,
            }))),
            ResponseObject::Error {
                jsonrpc: _,
                error,
                id: _,
            } => Err(ServerErrors::ApiError(error)),
        },
        ResponseObjects::Many(_) => todo!(),
        ResponseObjects::Empty => todo!(),
    }
}

impl RpcServer {
    pub fn new<I>(interface: Arc<I>) -> Self
    where
        I: NetworkInterface,
    {
        let server = Server::new()
            .with_data(Data::new(interface))
            .with_method("ursa_get_cid", network::get_cid_handler::<I>)
            .with_method("ursa_get_file", network::get_file_handler::<I>)
            .with_method("ursa_put_file", network::put_file_handler::<I>)
            .with_method("ursa_get_peers", network::get_peers::<I>)
            .with_method(
                "ursa_listener_addresses",
                network::get_listener_addresses::<I>,
            );

        RpcServer(server.finish())
    }
}

pub mod api_server;
mod error;
mod router;

mod bucket;
mod cluster;
mod key;

use hyper::{Body, Request};
use serde::Deserialize;

use error::*;

pub async fn parse_json_body<T: for<'de> Deserialize<'de>>(req: Request<Body>) -> Result<T, Error> {
	let body = hyper::body::to_bytes(req.into_body()).await?;
	let resp: T = serde_json::from_slice(&body).ok_or_bad_request("Invalid JSON")?;
	Ok(resp)
}
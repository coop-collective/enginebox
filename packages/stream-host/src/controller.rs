use anyhow::{Context, Result, bail};
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::time::Duration;

#[derive(Clone)]
pub struct Controller {
    client: Client,
    origin: String,
    token: String,
    epoch: Option<u64>,
    session: String,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Session {
    pub id: String,
    pub project: String,
    pub node_id: String,
    pub node_epoch: u64,
    pub mode: String,
    pub state: String,
    pub expires: u64,
    #[serde(default)]
    pub ice_servers: Vec<IceServer>,
    pub ice_transport_policy: Option<String>,
    pub ice_expires: Option<u64>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IceServer {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: String,
    #[serde(default)]
    pub credential: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Poll {
    pub sessions: Vec<Session>,
    pub now: u64,
}
#[derive(Deserialize)]
pub struct Signals {
    pub signals: Vec<Signal>,
    pub cursor: u64,
    pub session: Session,
}
#[derive(Deserialize)]
pub struct Signal {
    pub sequence: u64,
    pub signal: Value,
}
#[derive(Debug)]
pub struct AuthorityLost;
#[derive(Debug)]
pub struct SessionGone;
impl std::fmt::Display for SessionGone {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "stream session has ended")
    }
}
impl std::error::Error for SessionGone {}
impl std::fmt::Display for AuthorityLost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "controller rejected this host identity or process epoch")
    }
}
impl std::error::Error for AuthorityLost {}
impl Controller {
    pub fn new(origin: String, token: String) -> Result<Self> {
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .connect_timeout(Duration::from_secs(5))
                .timeout(Duration::from_secs(10))
                .build()?,
            origin: origin.trim_end_matches('/').into(),
            token,
            epoch: None,
            session: uuid::Uuid::new_v4().to_string(),
        })
    }
    pub async fn request<T: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<T> {
        let mut request = self
            .client
            .request(method, format!("{}{path}", self.origin))
            .bearer_auth(&self.token);
        if let Some(epoch) = self.epoch {
            request = request
                .header("x-enginebox-epoch", epoch)
                .header("x-enginebox-session", &self.session);
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let mut response = request.send().await.context("controller request failed")?;
        if response.status() == StatusCode::GONE {
            return Err(SessionGone.into());
        }
        if matches!(
            response.status(),
            StatusCode::UNAUTHORIZED | StatusCode::CONFLICT | StatusCode::FORBIDDEN
        ) {
            return Err(AuthorityLost.into());
        }
        if !response.status().is_success() {
            bail!("controller returned HTTP{}", response.status().as_u16())
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if bytes.len() + chunk.len() > 1024 * 1024 {
                bail!("controller response exceeds1MiB")
            }
            bytes.extend_from_slice(&chunk);
        }
        serde_json::from_slice(&bytes).context("invalid controller response")
    }
    pub async fn enroll(&self, hash: &str) -> Result<()> {
        let _: Value = self
            .request(
                Method::POST,
                "/v1/node/enroll",
                Some(json!({"nodeSecretHash":hash})),
            )
            .await?;
        Ok(())
    }
    pub async fn hello(&mut self) -> Result<u64> {
        let v: Value = self
            .request(
                Method::POST,
                "/v1/node/hello",
                Some(json!({"session":self.session})),
            )
            .await?;
        let epoch = v["epoch"].as_u64().context("missing node epoch")?;
        if v["session"].as_str() != Some(&self.session) {
            bail!("controller returned another process session")
        }
        self.epoch = Some(epoch);
        Ok(epoch)
    }
    pub async fn heartbeat(&self, peers: usize, frames: u64, inputs: u64) -> Result<()> {
        let _:Value=self.request(Method::POST,"/v1/node/heartbeat",Some(json!({"stats":{"webrtc":1,"video":1,"audio":0,"input":1,"peers":peers,"framesEncoded":frames,"inputsForwarded":inputs}}))).await?;
        Ok(())
    }
    pub async fn poll(&self) -> Result<Poll> {
        self.request(
            Method::POST,
            "/v1/node/machine-sessions/poll",
            Some(json!({})),
        )
        .await
    }
    pub async fn signals(&self, session: &Session, cursor: u64) -> Result<Signals> {
        self.request(
            Method::GET,
            &format!("{}/signals?after={cursor}", session_path(session)?),
            None,
        )
        .await
    }
    pub async fn answer(&self, session: &Session, id: &str, sdp: &str) -> Result<()> {
        let _: Value = self
            .request(
                Method::POST,
                &format!("{}/signals", session_path(session)?),
                Some(json!({"id":id,"signal":{"type":"answer","sdp":sdp}})),
            )
            .await?;
        Ok(())
    }
    pub async fn end(&self, session: &Session) -> Result<()> {
        let _: Value = self
            .request(
                Method::POST,
                &format!("{}/end", session_path(session)?),
                Some(json!({"id":format!("end-{}",uuid::Uuid::new_v4())})),
            )
            .await?;
        Ok(())
    }
}
fn session_path(session: &Session) -> Result<String> {
    for s in [&session.project, &session.id] {
        if s.is_empty()
            || s.len() > 128
            || !s
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
        {
            bail!("invalid controller session path")
        }
    }
    Ok(format!(
        "/v1/projects/{}/machine-sessions/{}",
        session.project, session.id
    ))
}

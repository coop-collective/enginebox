use crate::model::{Job, Request, repository};
use anyhow::{Context, Result, ensure};
use reqwest::blocking::Client;
use serde_json::{Value, json};
use std::{io::Read, time::Duration};

pub struct Github {
    client: Client,
    token: String,
    base: String,
}

impl Github {
    pub fn from_env() -> Result<Self> {
        Self::new(
            std::env::var("ENGINEBOX_CI_GITHUB_TOKEN")
                .context("Configure ENGINEBOX_CI_GITHUB_TOKEN on the controller")?,
        )
    }
    pub fn new(token: String) -> Result<Self> {
        ensure!(
            !token.is_empty() && token.bytes().all(|b| (33..=126).contains(&b)),
            "Invalid GitHub token"
        );
        Ok(Self {
            client: Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .no_proxy()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()?,
            token,
            base: "https://api.github.com".into(),
        })
    }
    fn request(&self, path: &str, body: Option<Value>) -> Result<(u16, Value)> {
        let url = format!("{}{path}", self.base);
        let request = if let Some(body) = body {
            self.client.post(url).json(&body)
        } else {
            self.client.get(url)
        };
        let response = request.bearer_auth(&self.token).header("Accept", "application/vnd.github+json").header("User-Agent", "enginebox-ci").header("X-GitHub-Api-Version", "2026-03-10").send().map_err(|_| anyhow::anyhow!("GitHub request outcome is uncertain; inspect GitHub before submitting a new request ID"))?;
        let status = response.status();
        ensure!(
            status.is_success(),
            "GitHub returned HTTP {}; no automatic retry was made",
            status.as_u16()
        );
        let mut data = Vec::new();
        response.take(1024 * 1024 + 1).read_to_end(&mut data)?;
        ensure!(data.len() <= 1024 * 1024, "GitHub response too large");
        let value = if data.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&data).context("Invalid GitHub response")?
        };
        Ok((status.as_u16(), value))
    }
    pub fn dispatch(&self, request: &Request) -> Result<Value> {
        request.validate()?;
        let Job::GithubActions {
            repository,
            workflow,
            git_ref,
            inputs,
        } = &request.job
        else {
            anyhow::bail!("Expected GitHub Actions job");
        };
        let mut inputs = inputs.clone();
        inputs.insert("enginebox_sha".into(), request.sha.clone());
        let (status, value) = self.request(
            &format!("/repos/{repository}/actions/workflows/{workflow}/dispatches"),
            Some(json!({"ref":git_ref,"inputs":inputs})),
        )?;
        ensure!(
            [200, 204].contains(&status),
            "Unexpected GitHub dispatch response"
        );
        let id = value["workflow_run_id"].as_u64();
        Ok(
            json!({"runner":"github-actions","status":"dispatched","completed":false,"repository":repository,"workflow":workflow,"sha":request.sha,"run_id":id,"run_url":id.map(|id|format!("https://github.com/{repository}/actions/runs/{id}")),"note":"Dispatch acceptance is not CI success. Read the workflow run conclusion."}),
        )
    }
    pub fn status(&self, repo: &str, run: u64) -> Result<Value> {
        ensure!(repository(repo) && run > 0, "Invalid GitHub run");
        let (_, value) = self.request(&format!("/repos/{repo}/actions/runs/{run}"), None)?;
        ensure!(value["id"] == run, "GitHub returned another run");
        Ok(
            json!({"runner":"github-actions","repository":repo,"run_id":run,"status":value["status"],"conclusion":value["conclusion"],"run_url":format!("https://github.com/{repo}/actions/runs/{run}")}),
        )
    }
    pub fn cancel(&self, repo: &str, run: u64) -> Result<Value> {
        ensure!(repository(repo) && run > 0, "Invalid GitHub run");
        self.request(
            &format!("/repos/{repo}/actions/runs/{run}/cancel"),
            Some(json!({})),
        )?;
        Ok(json!({"run_id":run,"cancellation_requested":true}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io::{BufRead, BufReader, Write},
        net::TcpListener,
        thread,
    };

    fn server(status: &str, body: &str) -> (Github, thread::JoinHandle<(String, Value)>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut github = Github::new("fixture-token".into()).unwrap();
        github.base = format!("http://{}", listener.local_addr().unwrap());
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut headers = String::new();
            let mut size = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    size = value.trim().parse::<usize>().unwrap();
                }
                headers.push_str(&line);
            }
            let mut body = vec![0; size];
            reader.read_exact(&mut body).unwrap();
            stream.write_all(response.as_bytes()).unwrap();
            (
                headers,
                if body.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&body).unwrap()
                },
            )
        });
        (github, handle)
    }
    fn trigger() -> Request {
        serde_json::from_value(json!({"schema_version":1,"request_id":"merge-ci","event":"merge","sha":"a".repeat(40),"job":{"runner":"github-actions","repository":"owner/repo","workflow":"ios.yml","ref":"main","inputs":{"scheme":"App"}}})).unwrap()
    }
    #[test]
    fn dispatch_preserves_commit_and_returns_external_run_without_false_green() {
        let (github, server) = server(
            "200 OK",
            r#"{"workflow_run_id":42,"html_url":"https://untrusted.example"}"#,
        );
        let result = github.dispatch(&trigger()).unwrap();
        assert_eq!(result["status"], "dispatched");
        assert_eq!(result["completed"], false);
        assert_eq!(
            result["run_url"],
            "https://github.com/owner/repo/actions/runs/42"
        );
        let (headers, body) = server.join().unwrap();
        assert!(
            headers.starts_with("POST /repos/owner/repo/actions/workflows/ios.yml/dispatches ")
        );
        assert!(headers.contains("Bearer fixture-token"));
        assert_eq!(body["ref"], "main");
        assert_eq!(body["inputs"]["enginebox_sha"], "a".repeat(40));
        assert_eq!(body["inputs"]["scheme"], "App");
        assert!(!result.to_string().contains("fixture-token"));
    }
    #[test]
    fn legacy_dispatch_and_failure_do_not_fabricate_a_run_or_leak_errors() {
        let (github, handle) = server("204 No Content", "");
        assert!(github.dispatch(&trigger()).unwrap()["run_id"].is_null());
        handle.join().unwrap();
        let (github, handle) = server("403 Forbidden", r#"{"message":"fixture-token"}"#);
        let error = github.dispatch(&trigger()).unwrap_err().to_string();
        assert!(error.contains("403") && !error.contains("fixture-token"));
        handle.join().unwrap();
    }
    #[test]
    fn status_and_cancel_use_the_exact_run() {
        let (github, handle) = server(
            "200 OK",
            r#"{"id":42,"status":"completed","conclusion":"failure"}"#,
        );
        assert_eq!(
            github.status("owner/repo", 42).unwrap()["conclusion"],
            "failure"
        );
        assert!(
            handle
                .join()
                .unwrap()
                .0
                .starts_with("GET /repos/owner/repo/actions/runs/42 ")
        );
        let (github, handle) = server("202 Accepted", "");
        assert_eq!(
            github.cancel("owner/repo", 42).unwrap()["cancellation_requested"],
            true
        );
        assert!(
            handle
                .join()
                .unwrap()
                .0
                .starts_with("POST /repos/owner/repo/actions/runs/42/cancel ")
        );
        assert!(github.status("owner/../repo", 42).is_err());
    }
}

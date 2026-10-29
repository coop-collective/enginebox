//! Opening a pull request through the repository GitHub App
//! (docs/PROJECT-CONFIG.md, "Generating a config"). The App needs the
//! repository permission **Pull requests: read and write**.
use super::{GitHub, api, full_name, text};
use anyhow::{Context, Result, ensure};
use serde_json::{Value, json};

/// The message when GitHub will not issue a pull-request token.
pub const PERMISSION_HINT: &str = "The GitHub App could not open a pull request; grant it the Pull requests: read and write repository permission";

impl GitHub {
    /// Open a pull request from `head` (a branch of the same repository) into
    /// the selected binding's branch, or return the one already open for it.
    /// `{number, url, reused}`.
    pub fn open_pull_request(
        &self,
        repository: &str,
        head: &str,
        title: &str,
        body: &str,
    ) -> Result<Value> {
        full_name(repository)?;
        ensure!(
            !head.is_empty()
                && head.len() <= 200
                && head
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/._-".contains(&b)),
            "Invalid branch name"
        );
        let selected = {
            let data = self.data.lock().unwrap();
            data.repositories
                .values()
                .find(|r| r.full_name == repository)
                .context("Select this repository in the GitHub panel first")?
                .clone()
        };
        let app = self.app("repos")?;
        let response = self
            .api
            .call(
                "POST",
                &format!(
                    "/app/installations/{}/access_tokens",
                    selected.installation_id
                ),
                &api::jwt(app)?,
                Some(json!({
                    "repository_ids": [selected.id],
                    "permissions": {"pull_requests": "write", "contents": "read"},
                })),
            )
            .context(PERMISSION_HINT)?;
        let token = text(&response, "token")?.to_owned();
        let owner = selected.full_name.split('/').next().unwrap_or("");
        let open = self.api.call(
            "GET",
            &format!(
                "/repos/{}/pulls?state=open&head={owner}:{head}&per_page=1",
                selected.full_name
            ),
            &token,
            None,
        )?;
        let view = |pr: &Value, reused: bool| -> Result<Value> {
            Ok(json!({
                "number": pr["number"].as_u64().context("Invalid GitHub pull request")?,
                "url": text(pr, "html_url")?,
                "reused": reused,
            }))
        };
        if let Some(pr) = open.as_array().and_then(|a| a.first()) {
            return view(pr, true);
        }
        let created = self.api.call(
            "POST",
            &format!("/repos/{}/pulls", selected.full_name),
            &token,
            Some(json!({"title": title, "head": head, "base": selected.branch, "body": body})),
        )?;
        view(&created, false)
    }
}

#[cfg(test)]
mod tests {
    use crate::Repository;
    use serde_json::json;

    #[test]
    fn opens_a_pull_request_or_reuses_the_open_one() {
        let (_root, mut g) = crate::tests::service();
        g.data.lock().unwrap().repositories.insert(
            102,
            Repository {
                id: 102,
                installation_id: 5,
                full_name: "fixture/site".into(),
                private: true,
                branch: "main".into(),
            },
        );
        crate::tests::with_app_key(&mut g);
        let pr = json!({"number": 7, "html_url": "https://github.com/fixture/site/pull/7"});
        let (api, server) = crate::tests::api_fixture(vec![
            (
                "/app/installations/5/access_tokens",
                json!({"token": "ghs_pr", "__request": {"body": {"permissions": {"pull_requests": "write", "contents": "read"}}}}),
            ),
            ("head=fixture:enginebox/agent-config", json!([])),
            (
                "/repos/fixture/site/pulls",
                json!({"number": 7, "html_url": "https://github.com/fixture/site/pull/7", "__request": {"authorization": "Bearer ghs_pr", "body": {"head": "enginebox/agent-config", "base": "main"}}}),
            ),
            (
                "/app/installations/5/access_tokens",
                json!({"token": "ghs_pr"}),
            ),
            ("head=fixture:enginebox/agent-config", json!([pr])),
        ]);
        g.api = api;
        let first = g
            .open_pull_request("fixture/site", "enginebox/agent-config", "t", "b")
            .unwrap();
        assert_eq!(
            first,
            json!({"number": 7, "url": "https://github.com/fixture/site/pull/7", "reused": false})
        );
        let again = g
            .open_pull_request("fixture/site", "enginebox/agent-config", "t", "b")
            .unwrap();
        assert_eq!(again["reused"], true);
        server.join().unwrap();
        assert!(
            g.open_pull_request("fixture/site", "bad branch", "t", "b")
                .is_err()
        );
    }
}

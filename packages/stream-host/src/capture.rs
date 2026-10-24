use crate::{config::Config, input::Input};
use anyhow::{Context, Result, bail};
use bytes::Bytes;
use std::{
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    process::{Child, Command},
    sync::broadcast,
    task::JoinHandle,
};

pub struct Capture {
    children: Vec<OwnedChild>,
    tasks: Vec<JoinHandle<()>>,
    pub frames: broadcast::Sender<Bytes>,
    pub input: Arc<Mutex<Input>>,
    pub failed: Arc<AtomicBool>,
    directory: PathBuf,
}
struct OwnedChild {
    child: Child,
    group: Option<i32>,
}
impl OwnedChild {
    fn new(child: Child) -> Self {
        let group = child.id().map(|id| id as i32);
        Self { child, group }
    }
    fn signal(&self, signal: i32) {
        if let Some(group) = self.group {
            unsafe {
                libc::kill(-group, signal);
            }
        }
    }
    fn group_exists(&self) -> bool {
        self.group
            .is_some_and(|group| unsafe { libc::kill(-group, 0) == 0 })
    }
}
impl std::ops::Deref for OwnedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}
impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        self.signal(libc::SIGKILL);
    }
}
fn command(program: &str, directory: &std::path::Path, display: &str) -> Command {
    let mut c = Command::new(program);
    c.env_clear()
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("HOME", directory)
        .env("XDG_RUNTIME_DIR", directory)
        .env("DISPLAY", display)
        .env("LIBGL_ALWAYS_SOFTWARE", "1")
        .env("LANG", "C.UTF-8")
        .env("DOTNET_CLI_HOME", directory)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::null())
        .kill_on_drop(true)
        .process_group(0);
    c
}
impl Capture {
    pub async fn start(
        config: &Config,
        count: Arc<AtomicU64>,
        inputs: Arc<AtomicU64>,
    ) -> Result<Self> {
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("enginebox-stream-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        // Always start our own display. Never read DISPLAY or attach to a host X socket.
        let display_number = 1000 + u16::from_le_bytes(rand::random::<[u8; 2]>()) % 20000;
        let display = format!(":{display_number}");
        if std::path::Path::new(&format!("/tmp/.X11-unix/X{display_number}")).exists() {
            bail!("private display collision; restart to choose another")
        }
        let mut children = Vec::new();
        let start = async {
            children.push(OwnedChild::new(
                command("/usr/bin/Xvfb", &directory, &display)
                    .args([
                        &display,
                        "-screen",
                        "0",
                        &format!("{}x{}x24", config.width, config.height),
                        "-nolisten",
                        "tcp",
                        "-ac",
                        "-noreset",
                    ])
                    .spawn()
                    .context("start owned Xvfb")?,
            ));
            let input = tokio::time::timeout(Duration::from_secs(8), async {
                loop {
                    if children[0].try_wait()?.is_some() {
                        bail!("owned display exited before readiness")
                    }
                    if let Ok(input) =
                        Input::connect(&display, config.width, config.height, inputs.clone())
                    {
                        return Ok(input);
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            })
            .await
            .context("owned display readiness timed out")??;
            let mut engine = command(
                config
                    .command
                    .executable
                    .to_str()
                    .context("command path is not UTF8")?,
                &directory,
                &display,
            );
            engine
                .args(&config.command.args)
                .current_dir(&config.command.directory)
                .envs(&config.command.env);
            children.push(OwnedChild::new(
                engine.spawn().context("start configured engine command")?,
            ));
            let mut encoder = command("/usr/bin/ffmpeg", &directory, &display)
                .args([
                    "-hide_banner",
                    "-loglevel",
                    "error",
                    "-f",
                    "x11grab",
                    "-video_size",
                    &format!("{}x{}", config.width, config.height),
                    "-framerate",
                    "30",
                    "-draw_mouse",
                    "0",
                    "-i",
                    &format!("{display}.0"),
                    "-an",
                    "-c:v",
                    "libvpx",
                    "-deadline",
                    "realtime",
                    "-cpu-used",
                    "8",
                    "-threads",
                    "2",
                    "-b:v",
                    "2M",
                    "-g",
                    "30",
                    "-fps_mode",
                    "passthrough",
                    "-f",
                    "ivf",
                    "-flush_packets",
                    "1",
                    "pipe:1",
                ])
                .stdout(Stdio::piped())
                .spawn()
                .context("start VP8 encoder")?;
            let stdout = encoder.stdout.take().context("encoder output missing")?;
            children.push(OwnedChild::new(encoder));
            Ok::<_, anyhow::Error>((input, stdout))
        }
        .await;
        let (input, stdout) = match start {
            Ok(v) => v,
            Err(e) => {
                stop_children(&mut children).await;
                let _ = std::fs::remove_dir_all(&directory);
                return Err(e);
            }
        };
        let (frames, _) = broadcast::channel(2);
        let failed = Arc::new(AtomicBool::new(false));
        let (f, s) = (failed.clone(), frames.clone());
        let task = tokio::spawn(async move {
            if read_frames(stdout, s, count).await.is_err() {
                f.store(true, Ordering::SeqCst);
            }
        });
        Ok(Self {
            children,
            tasks: vec![task],
            frames,
            input: Arc::new(Mutex::new(input)),
            failed,
            directory,
        })
    }
    pub fn healthy(&mut self) -> bool {
        !self.failed.load(Ordering::SeqCst)
            && self
                .children
                .iter_mut()
                .all(|c| matches!(c.try_wait(), Ok(None)))
    }
    pub async fn stop(mut self) {
        self.input.lock().unwrap().release_all();
        for task in &self.tasks {
            task.abort();
        }
        stop_children(&mut self.children).await;
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
        for child in &self.children {
            child.signal(libc::SIGKILL);
        }
    }
}
async fn stop_children(children: &mut [OwnedChild]) {
    for child in children.iter().rev() {
        child.signal(libc::SIGTERM);
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        for child in children.iter_mut() {
            let _ = child.try_wait();
        }
        if !children.iter().any(OwnedChild::group_exists) || tokio::time::Instant::now() >= deadline
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    // Retain the original PGID even after try_wait reaps its leader. Descendants
    // can still occupy that owned group and may have ignored TERM.
    for child in children.iter().rev() {
        child.signal(libc::SIGKILL);
    }
    for child in children.iter_mut().rev() {
        let _ = tokio::time::timeout(Duration::from_secs(1), child.wait()).await;
        child.group = None;
    }
}
async fn read_frames(
    mut out: tokio::process::ChildStdout,
    frames: broadcast::Sender<Bytes>,
    count: Arc<AtomicU64>,
) -> Result<()> {
    let mut header = [0; 32];
    out.read_exact(&mut header).await?;
    if &header[..4] != b"DKIF" || &header[8..12] != b"VP80" {
        bail!("encoder did not produce VP8 IVF")
    }
    loop {
        let mut h = [0; 12];
        out.read_exact(&mut h).await?;
        let size = u32::from_le_bytes(h[..4].try_into().unwrap()) as usize;
        if size == 0 || size > 4 * 1024 * 1024 {
            bail!("encoded frame exceeds bound")
        }
        let mut b = vec![0; size];
        out.read_exact(&mut b).await?;
        let _ = frames.send(Bytes::from(b));
        count.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cleanup_kills_descendant_after_its_direct_parent_exits() -> Result<()> {
        let dir = tempfile::tempdir()?;
        let pidfile = dir.path().join("descendant.pid");
        let child = Command::new("/bin/sh")
            .args([
                "-c",
                "trap '' TERM; sleep 30 & echo $! > \"$1\"; exit 0",
                "fixture",
            ])
            .arg(&pidfile)
            .process_group(0)
            .spawn()?;
        let mut children = vec![OwnedChild::new(child)];
        children[0].wait().await?;
        assert!(children[0].id().is_none());
        let pid: i32 = std::fs::read_to_string(pidfile)?.trim().parse()?;
        assert!(children[0].group_exists());
        stop_children(&mut children).await;
        let stopped = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                // In the container test the harness is PID1 and adopts this child.
                let mut status = 0;
                unsafe {
                    libc::waitpid(pid, &mut status, libc::WNOHANG);
                }
                if unsafe { libc::kill(pid, 0) } != 0 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            stopped.is_ok(),
            "owned descendant survived process-group cleanup"
        );
        Ok(())
    }
}

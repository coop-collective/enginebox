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
    children: Vec<Child>,
    tasks: Vec<JoinHandle<()>>,
    pub frames: broadcast::Sender<Bytes>,
    pub input: Arc<Mutex<Input>>,
    pub failed: Arc<AtomicBool>,
    directory: PathBuf,
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
            children.push(
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
            );
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
            children.push(engine.spawn().context("start configured engine command")?);
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
            children.push(encoder);
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
        for child in &mut self.children {
            if let Some(pid) = child.id() {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
        }
    }
}
async fn stop_children(children: &mut [Child]) {
    for child in children.iter_mut().rev() {
        if let Some(pid) = child.id() {
            unsafe {
                libc::kill(-(pid as i32), libc::SIGTERM);
            }
        }
    }
    for child in children.iter_mut().rev() {
        let pid = child.id();
        if tokio::time::timeout(Duration::from_secs(2), child.wait())
            .await
            .is_err()
        {
            if let Some(pid) = pid {
                unsafe {
                    libc::kill(-(pid as i32), libc::SIGKILL);
                }
            }
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
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

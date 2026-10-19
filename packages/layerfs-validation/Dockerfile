FROM node:24.21.0-bookworm@sha256:3d27e5c11e5786e309ec3e03f93ae536eb36e6e5eb3714d5eb3300a36157add0
RUN apt-get update && apt-get install -y --no-install-recommends git fuse3 inotify-tools ca-certificates \
 && rm -rf /var/lib/apt/lists/* \
 && npm install --global pnpm@12.14.0
ENV CI=1
CMD ["bash"]

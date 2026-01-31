FROM node:20

ARG BUILDOS
ARG BUILDARCH
ARG TZ
ARG CLAUDE_CODE_VERSION
ARG GIT_DELTA_VERSION
ARG ZSH_IN_DOCKER_VERSION
ARG RUST_VERSION
ARG PYTHON_VERSION
ARG GO_VERSION

ENV TZ="$TZ"

# Install build dependencies needed for further installations
RUN apt-get update && apt-get install -y --no-install-recommends \
  wget \
  curl \
  ca-certificates \
  build-essential \
  pkg-config \
  gnupg2 \
  python${PYTHON_VERSION}-dev \
  sudo \
  && apt-get clean && rm -rf /var/lib/apt/lists/*

# Ensure default node user has access to /usr/local/share
RUN mkdir -p /usr/local/share/npm-global && \
  chown -R node:node /usr/local/share

ARG USERNAME=node

# Persist bash history.
RUN SNIPPET="export PROMPT_COMMAND='history -a' && export HISTFILE=/commandhistory/.bash_history" \
  && mkdir /commandhistory \
  && touch /commandhistory/.bash_history \
  && chown -R $USERNAME /commandhistory

# Set `DEVCONTAINER` environment variable to help with orientation
ENV DEVCONTAINER=true

# Create workspace and config directories and set permissions
RUN mkdir -p /workspace /home/node/.claude && \
  chown -R node:node /workspace /home/node/.claude

# Remove existing Golang installation, if any, and set permissions
RUN rm -rf /usr/local/go ; mkdir -p /usr/local/go && \
  chown node:node /usr/local /usr/local/go

WORKDIR /workspace

RUN ARCH=$(dpkg --print-architecture) && \
  wget "https://github.com/dandavison/delta/releases/download/${GIT_DELTA_VERSION}/git-delta_${GIT_DELTA_VERSION}_${ARCH}.deb" && \
  sudo dpkg -i "git-delta_${GIT_DELTA_VERSION}_${ARCH}.deb" && \
  rm "git-delta_${GIT_DELTA_VERSION}_${ARCH}.deb"

# Set up non-root user
USER node

# Install Rust
ENV RUSTUP_HOME=/home/node/.rustup \
    CARGO_HOME=/home/node/.cargo
ENV PATH=$PATH:/home/node/.cargo/bin

RUN curl https://sh.rustup.rs -sSf | sh -s -- \
    -y \
    --default-toolchain ${RUST_VERSION} \
    --profile minimal \
 && rustup component add rustfmt clippy

# Install Python

ENV UV_HOME=/home/node/.uv
ENV PATH=$PATH:/home/node/.cargo/bin:/home/node/.local/bin

RUN curl -Ls https://astral.sh/uv/install.sh | sh
RUN uv python install ${PYTHON_VERSION}

# Install Go

RUN  curl -LsSfo go${GO_VERSION}.${BUILDOS}-${BUILDARCH}.tar.gz https://go.dev/dl/go${GO_VERSION}.${BUILDOS}-${BUILDARCH}.tar.gz && \
     tar -C /usr/local -xzf go${GO_VERSION}.${BUILDOS}-${BUILDARCH}.tar.gz && \
     rm go${GO_VERSION}.${BUILDOS}-${BUILDARCH}.tar.gz

ENV PATH=$PATH:/usr/local/go/bin

# Check installation

RUN rustc --version \
 && cargo --version \
 && uv --version \
 && go version

# Install global packages
ENV NPM_CONFIG_PREFIX=/usr/local/share/npm-global
ENV PATH=$PATH:/usr/local/share/npm-global/bin

# Set the default shell to zsh rather than sh
ENV SHELL=/bin/zsh

# Set the default editor and visual
ENV EDITOR=vim
ENV VISUAL=vim

# Switch back to root to install runtime dependencies
USER root

# Install runtime dependencies for final image
RUN apt-get update && apt-get install -y --no-install-recommends \
  traceroute \
  iputils-ping \
  sqlite3 \
  less \
  git \
  procps \
  fzf \
  zsh \
  man-db \
  unzip \
  gh \
  iptables \
  ipset \
  iproute2 \
  dnsutils \
  aggregate \
  jq \
  nano \
  vim \
  && apt-get clean && rm -rf /var/lib/apt/lists/*

# Switch back to node user
USER node

# Default powerline10k theme
RUN sh -c "$(wget -O- https://github.com/deluan/zsh-in-docker/releases/download/v${ZSH_IN_DOCKER_VERSION}/zsh-in-docker.sh)" -- \
  -p git \
  -p fzf \
  -a "source /usr/share/doc/fzf/examples/key-bindings.zsh" \
  -a "source /usr/share/doc/fzf/examples/completion.zsh" \
  -a "export PROMPT_COMMAND='history -a' && export HISTFILE=/commandhistory/.bash_history" \
  -x

# Install Claude
RUN npm install -g @anthropic-ai/claude-code@${CLAUDE_CODE_VERSION}

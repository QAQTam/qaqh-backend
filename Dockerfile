# QAQ-Harness 主仓 NPC 运行环境
# Ubuntu 26.04 LTS + Rust 1.98.1（对齐 owner 本机工具链）+ gcc-15 + cmake
# 构建后推送 CNB 制品库，供 issue.comment@npc / pull_request.comment@npc 事件引用。
# 运行时清单对照官方 npc:go 文档：git/git-lfs/curl/jq/ripgrep + cnb-cli + skills/cnb-skill。
FROM ubuntu:26.04

ENV DEBIAN_FRONTEND=noninteractive

# 基础工具链 + NPC 运行时依赖
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        ca-certificates \
        build-essential \
        gcc-15 \
        g++-15 \
        cmake \
        git \
        git-lfs \
        curl \
        jq \
        ripgrep \
        pkg-config \
        file \
        unzip \
        xz-utils \
        tzdata \
    && rm -rf /var/lib/apt/lists/* \
    && git lfs install

# Node 22（cnb-cli / skills 运行时）
RUN curl -fsSL https://deb.nodesource.com/setup_22.x | bash - \
    && apt-get install -y --no-install-recommends nodejs \
    && rm -rf /var/lib/apt/lists/* \
    && node -v && npm -v

# Rust 1.98.1 固定工具链（与 owner 本机 cargo 1.98.1 对齐）
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:${PATH}
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --no-modify-path --profile minimal --default-toolchain 1.98.1 \
    && rustup component add clippy rustfmt \
    && cargo --version && rustc --version

# NPC 运行时：CNB CLI + Skills 框架 + 官方 cnb-skill
RUN npm install -g @cnbcool/cnb-cli skills \
    && npx skills add https://cnb.cool/cnb/skills/cnb-skill.git -g -y \
    && cnb --version || true

ENV TZ=Asia/Shanghai
RUN ln -snf /usr/share/zoneinfo/$TZ /etc/localtime && echo $TZ > /etc/timezone

# 冒烟断言：版本不对即 fail 构建
RUN cargo --version | grep -q 1.98.1 \
    && gcc-15 --version | head -1 \
    && cmake --version | head -1 \
    && bash --version | head -1 \
    && node -v \
    && echo "[qaqh-npc-env] smoke check passed"

CMD ["/bin/bash"]

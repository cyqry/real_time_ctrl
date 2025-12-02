#docker build -t rust-1.80-mine /home
FROM rust:1.80-bookworm
# 可以使用rust:1.80-alpine默认musl，但是下面命令的语法可能要变

# 安装必要的工具（单行写法）
# RUN apt-get update && apt-get install -y openssh-client && rm -rf /var/lib/apt/lists/*
RUN apt-get update && apt-get install -y musl-tools
RUN rustup target add x86_64-unknown-linux-musl


# 与jenkins服务所用用户要保持一致
ARG USER_ID=117
ARG GROUP_ID=123

# 创建用户和组（单行写法）
RUN groupadd -g $GROUP_ID jenkinsgroup && useradd -r -u $USER_ID -g jenkinsgroup -m -d /home/jenkins-docker -s /bin/bash jenkins-docker

CMD ["bash"]
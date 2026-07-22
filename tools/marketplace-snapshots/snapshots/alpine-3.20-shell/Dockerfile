# Minimal Alpine shell environment — busybox, coreutils, bash,
# curl, jq. Target for agent tool calls that need a POSIX shell
# without a language runtime.
FROM alpine:3.20@sha256:beefdbd8a1da6d2915566fde36db9db0b524eb737fc57cd1367effd16dc0d06d
RUN apk add --no-cache bash coreutils curl jq
CMD ["/bin/sh"]

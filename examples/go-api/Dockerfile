# A Go service built in a multi-stage Dockerfile; the final image is just the
# static binary on `scratch`, running as an unprivileged user.
#
# The build runs on the server, so the laptop or CI runner needs no Go
# toolchain. Every FROM has an explicit tag (or is `scratch`), so the build
# uses cached base images; an untagged or `:latest` FROM would add --pull.

# mypaas.service=go-api
# mypaas.url=https://go.example.com
# mypaas.port=8080
# mypaas.scale=2 roll
# mypaas.healthcheck=/healthz 5s 1s
# mypaas.readonly=true
# mypaas.maxmem=64m
# mypaas.maxcpu=0.5

FROM golang:1.25-alpine AS build
WORKDIR /src
COPY go.mod main.go ./
RUN CGO_ENABLED=0 go build -trimpath -ldflags="-s -w" -o /out/server .

FROM scratch
COPY --from=build /out/server /server
USER 65532:65532
ENTRYPOINT ["/server"]

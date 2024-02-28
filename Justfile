all: build

build:
  DOCKER_BUILDKIT=1 docker build -t docker.io/faux/begonia-proxy .

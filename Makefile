TARGET_HOST ?= bedroomblindspi
TARGET_USERNAME ?= pi
TARGET_HOST_USER ?= $(TARGET_USERNAME)@$(TARGET_HOST)

DEB_BUILD_PATH ?= target/debian/wakeword*.deb

.PHONY: build
build:
	cargo build --release

.PHONY: build-deb
build-deb: build
	cargo deb --no-build

.PHONE: install
install: build-deb
	sudo dpkg -i $(DEB_BUILD_PATH)

.PHONY: install-dependencies
install-dependencies:
	cargo install cargo-deb

.PHONY: build-docker
build-docker:
	rm -rf docker_out
	mkdir docker_out
	DOCKER_BUILDKIT=1 docker build --tag wakeword-builder --file Dockerfile --output type=local,dest=docker_out .

.PHONY: push-docker-built
push-docker-built: build-docker
	rsync -avz --delete docker_out/* $(TARGET_HOST_USER):/home/$(TARGET_USERNAME)/wakeword

TARGET_HOST ?= bedroomblindspi
TARGET_USERNAME ?= pi
TARGET_HOST_USER ?= $(TARGET_USERNAME)@$(TARGET_HOST)

DEB_BUILD_PATH ?= target/debian/wakeword*.deb

.PHONY: build
build:
	cargo build --release

.PHONY: copy-include-files
copy-include-files:
	rm -rf include
	mkdir -p include/default_keyword_files/
	cp target/release/build/pv_cobra-*/out/lib/raspberry-pi/cortex-a72-aarch64/libpv_cobra.so include/libpv_cobra.so
	cp target/release/build/pv_porcupine-*/out/lib/raspberry-pi/cortex-a72-aarch64/libpv_porcupine.so include/libpv_porcupine.so
	cp target/release/build/pv_recorder-*/out/lib/raspberry-pi/cortex-a72-aarch64/libpv_recorder.so include/libpv_recorder.so
	cp target/release/build/pv_porcupine-*/out/lib/common/porcupine_params.pv include/porcupine_params.pv
	cp target/release/build/pv_porcupine-*/out/resources/keyword_files/raspberry-pi/* include/default_keyword_files/

.PHONY: build-deb
build-deb: build copy-include-files
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

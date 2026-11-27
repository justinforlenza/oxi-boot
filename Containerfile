FROM scratch
ARG TARGETARCH
COPY dist/${TARGETARCH} /opt/oxi-boot

ENTRYPOINT ["/opt/oxi-boot/oxi-boot", "/etc/oxi-boot.toml"]

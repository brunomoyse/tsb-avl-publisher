# Stage 1: Build stage
FROM rust:1.86-alpine3.21 AS builder

# Install required dependencies, including nasm
RUN apk add --no-cache musl-dev openssl-dev openssl-libs-static pkgconfig nasm protobuf

WORKDIR /app

# Copy source files and manifest files.
COPY ./src ./src
COPY Cargo.toml Cargo.lock build.rs ./
COPY proto ./proto

# Build the application in release mode.
RUN cargo build --release

# Stage 2: Runtime stage
FROM alpine:3.21

# Install only necessary runtime dependencies.
RUN apk add --no-cache ca-certificates

# Copy the compiled binary from the builder stage.
COPY --from=builder /app/target/release/tsb-avl-publisher /usr/local/bin/

WORKDIR /usr/local/bin

# Switch to a non-root user (uid 65534)
USER 65534

# Command to run the executable.
CMD ["./tsb-avl-publisher"]
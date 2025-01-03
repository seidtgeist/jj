.PHONY: all
all: build clean run ls

.PHONY: run
run:
	RUST_BACKTRACE=1 WASMTIME_BACKTRACE_DETAILS=1 wasmtime run --dir /tmp ../../target/wasm32-wasip1/debug/jj-lib-wasm.wasm


.PHONY: build
build:
	cargo build --target wasm32-wasip1

.PHONY: clean
clean:
	rm -rf /tmp/jj*

.PHONY: ls
ls:
	lsd --tree /tmp/jj*

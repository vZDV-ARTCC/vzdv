default: build

build:
  cargo b
  cargo +nightly clippy
  cargo t

test-js:
  node --test vzdv-site/js-tests/ids_page.test.js

build-release:
  cargo b --release --all-features

deploy-static:
  rsync -avz ./static zdv-droplet-1:/home/zdv/vzdv/

deploy bin: build-release
  scp target/release/vzdv-{{bin}} zdv-droplet-1:/home/zdv/vzdv/vzdv-{{bin}}.new

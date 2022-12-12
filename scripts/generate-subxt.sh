#!/bin/bash

set -e

cargo run --bin codegen -- --path ./utils/subxt/generated/src
cargo +nightly fmt -- --emit=files
if [[ -z $(git status -s) ]]
then
  echo "Generated subxt types are up to date"
else
  echo "Subxt types are outdated, please generate subxt types for the new runtime."
#  exit 1
fi
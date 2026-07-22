#!/bin/sh
set -eu

cd /workspace/sentinel
if python -m unittest -v checks; then
  printf '1\n' > /logs/verifier/reward.txt
else
  printf '0\n' > /logs/verifier/reward.txt
fi

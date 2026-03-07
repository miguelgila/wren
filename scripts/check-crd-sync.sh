#!/usr/bin/env bash
# Verify that Helm chart CRDs stay in sync with manifests/crds/.
# The spec: sections must be identical; metadata differences (Helm labels/annotations) are expected.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
MANIFEST_DIR="${REPO_ROOT}/manifests/crds"
HELM_CRD_DIR="${REPO_ROOT}/charts/wren/templates/crds"

errors=0

for manifest in "${MANIFEST_DIR}"/*.yaml; do
  name=$(basename "$manifest")
  helm_crd="${HELM_CRD_DIR}/${name}"

  if [[ ! -f "$helm_crd" ]]; then
    echo "ERROR: ${name} exists in manifests/crds/ but not in charts/wren/templates/crds/"
    errors=$((errors + 1))
    continue
  fi

  manifest_spec=$(sed -n '/^spec:/,$p' "$manifest")
  helm_spec=$(sed -n '/^spec:/,$p' "$helm_crd")

  if [[ "$manifest_spec" != "$helm_spec" ]]; then
    echo "ERROR: spec drift in ${name}"
    diff <(echo "$manifest_spec") <(echo "$helm_spec") || true
    errors=$((errors + 1))
  fi
done

if [[ "$errors" -gt 0 ]]; then
  echo "FAILED: ${errors} CRD(s) out of sync."
  exit 1
fi

echo "All CRDs in sync."

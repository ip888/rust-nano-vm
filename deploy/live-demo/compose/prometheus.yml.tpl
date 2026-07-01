# Prometheus config for the live-KVM demo. Rendered by `../up.sh` —
# `${NANOVM_FLY_HOST}` gets substituted with the Fly.io hostname
# (e.g. `nanovm-live-demo.fly.dev`) before Prometheus starts.
#
# The scrape target is a real internet endpoint over HTTPS. If your
# Fly app is in a private-network-only shape, you'll want to run
# Prometheus inside the Fly private network instead — this compose
# stack is for "watch the demo from my laptop."

global:
  scrape_interval: 15s
  evaluation_interval: 15s
  external_labels:
    env: live-demo

rule_files:
  - /etc/prometheus/alerts.yaml

scrape_configs:
  - job_name: nanovm
    metrics_path: /metrics
    scheme: https
    static_configs:
      - targets:
          - ${NANOVM_FLY_HOST}
        labels:
          instance: fly-machine

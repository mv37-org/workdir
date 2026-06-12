//! The example configuration emitted by `sandboxd gen-config`.

pub const EXAMPLE_CONFIG: &str = r#"# sandboxd configuration

[server]
bind = "0.0.0.0:8080"
public_domain = "sandboxes.example.com"
public_https = true
# public_port = 8080   # include in preview URLs when not behind 443/80 (e.g. a LAN)
data_dir = "/var/lib/workdir"

[node]
role = "all-in-one"        # "all-in-one" or "worker"
node_id = ""                # generated + persisted if empty
advertise_addr = ""         # internal address other nodes use; defaults to bind
total_memory_gb = 0.0       # auto-detected when 0
control_plane_url = ""      # workers only
join_token = ""             # workers only

[runtime]
kind = "firecracker"        # "firecracker" (Linux + /dev/kvm) or "mock" (dev)
firecracker_bin = "/usr/local/bin/firecracker"
jailer_bin = "/usr/local/bin/jailer"
kernel_image = "/var/lib/workdir/kernel/vmlinux"
images_dir = "/var/lib/workdir/images"
workspace_dir = "/var/lib/workdir/workspaces"
# Resume-latency tuning (roadmap Phase 2):
restore_mem_backend = "file"  # "file" (eager, page-cache-prewarmed) or "uffd" (lazy demand paging; handler is the remaining KVM-host increment)
prewarm_mem_cache = true       # warm the snapshot mem file into page cache just before a restore
cpu_template = ""              # e.g. "T2"/"C3" for snapshot portability across heterogeneous hosts
# Density (roadmap Phase 3):
shared_rootfs = false          # share one read-only base rootfs across all VMs (hardlinked → one host page-cache copy); the guest layers tmpfs+overlayfs for writes. ext4-ro base today; erofs+DAX is a future guest-kernel rebuild

[pricing]
default_unit_price_usd_hr = 0.009
monthly_node_cost_usd = 48.0
node_practical_units = 20

[pricing.image_multipliers]
base = 1.0
"node-python" = 1.1
browser = 2.5
"heavy-build" = 2.0
custom = 1.2

[hotpool]
enabled = true
warm_interval_seconds = 5
base_target = 2

[standby]
# When true, idle sandboxes are parked in perpetual standby (snapshot + free RAM
# + $0, auto-resume on next request) instead of stopped. Off by default; validate
# the snapshot/restore path on the node (POST /v1/benchmarks/run) before enabling.
enabled = false

[auth]
bootstrap_admin_key = ""    # set for reproducible installs; else generated once
bootstrap_org = "org_admin"
"#;

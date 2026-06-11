//! The example configuration emitted by `sandboxd gen-config`.

pub const EXAMPLE_CONFIG: &str = r#"# sandboxd configuration

[server]
bind = "0.0.0.0:8080"
public_domain = "sandboxes.example.com"
public_https = true
# public_port = 8080   # include in preview URLs when not behind 443/80 (e.g. a LAN)
data_dir = "/var/lib/sandboxd"

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
kernel_image = "/var/lib/sandboxd/kernel/vmlinux"
images_dir = "/var/lib/sandboxd/images"
workspace_dir = "/var/lib/sandboxd/workspaces"

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

[auth]
bootstrap_admin_key = ""    # set for reproducible installs; else generated once
bootstrap_org = "org_admin"
"#;

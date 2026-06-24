//! The example configuration emitted by `sandboxd gen-config`.

pub const EXAMPLE_CONFIG: &str = r#"# sandboxd configuration

[server]
bind = "0.0.0.0:8080"
public_domain = "sandboxes.example.com"
public_https = true
# public_port = 8080   # include in preview URLs when not behind 443/80 (e.g. a LAN)
# capacity_exhausted_message = "Please self-host. This hosted endpoint is not for production; it is a capacity-limited demo running on a couple of EX44s."
data_dir = "/var/lib/workdir"

[node]
role = "all-in-one"        # "all-in-one" or "worker"
node_id = ""                # generated + persisted if empty
advertise_addr = ""         # internal address other nodes use; defaults to bind
total_memory_gb = 0.0       # auto-detected when 0
control_plane_url = ""      # workers only
join_token = ""             # workers only
rpc_token = ""              # shared control-plane/worker token for /internal RPC

[runtime]
kind = "firecracker"        # "firecracker" (Linux + /dev/kvm) or "mock" (dev)
firecracker_bin = "/usr/local/bin/firecracker"
jailer_bin = "/usr/local/bin/jailer"
use_jailer = false          # true = run VMMs under jailer; requires root daemon
jailer_uid_base = 100000    # first uid/gid used for per-VM jailer isolation
kernel_image = "/var/lib/workdir/kernel/vmlinux"
images_dir = "/var/lib/workdir/images"
workspace_dir = "/var/lib/workdir/workspaces"
volumes_dir = "/var/lib/workdir/volumes"   # persistent-volume backing images (Phase 5)
# Resume-latency tuning (roadmap Phase 2):
restore_mem_backend = "file"  # "file" (eager, page-cache-prewarmed) or "uffd" (lazy demand paging; handler is the remaining KVM-host increment)
prewarm_mem_cache = true       # warm the snapshot mem file into page cache just before a restore
cpu_template = ""              # e.g. "T2"/"C3" for snapshot portability across heterogeneous hosts
quiet_guest_boot = true        # `quiet loglevel=1` on the guest cmdline: skip serial boot logging (a real share of cold boot); disable when debugging guest boots
jailer_pool_size = 0           # pre-spawned idle jailer+Firecracker processes; restores/golden boots claim one and skip the ~30ms relaunch (0 = off)
# Density (roadmap Phase 3):
shared_rootfs = false          # share one read-only base rootfs across all VMs (hardlinked → one host page-cache copy); the guest layers tmpfs+overlayfs for writes. ext4-ro base today; erofs+DAX is a future guest-kernel rebuild
balloon = false                # virtio-balloon on every VM: enables the soft-standby tier + per-VM guest memory stats
firecracker_no_seccomp = false # true adds --no-seccomp for snapshot/restore troubleshooting
require_reflink = false        # true = probe the data FS for reflink (FICLONE) at startup and FAIL CLOSED: fork/private-disk staging bail rather than silently emit a multi-GB full copy on a non-reflink FS (xfs/btrfs support it; ext4 needs -O reflink)

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
# Soft standby: after this many idle seconds (before snapshot eviction), inflate
# the guest balloon so free guest memory returns to the host — zero resume
# latency at a fraction of the RSS. 0 = off; requires runtime.balloon.
balloon_idle_seconds = 0

[capacity]
# Static admission (spec §9.1): memory reserved for the host, and the fraction
# of theoretical slots admitted. The defaults predate the shared rootfs and
# perpetual standby — measure, then tighten.
host_reserve_gb = 12.0
practical_derate = 0.7692
# Measured-memory overcommit: admit past the static shape-sum ceiling while the
# host still MEASURES at least overcommit_headroom_gb free beyond the request
# (guests fault pages lazily; idle sandboxes are parked at $0). Enable together
# with psi_standby_threshold — the pressure reaper is the backpressure.
overcommit = false
overcommit_headroom_gb = 8.0
# When memory PSI (some avg10) exceeds this percentage, park the least-recently-
# active running sandbox in standby ahead of its idle window. 0 = off.
psi_standby_threshold = 0.0

[auth]
bootstrap_admin_key = ""    # set for reproducible installs; else generated once
bootstrap_org = "org_admin"
"#;

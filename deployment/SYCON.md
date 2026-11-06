# prox-lxc1 / prox-lxc2 — Rust console LXCs (110, 111)

## Create containers (on each Proxmox host)

Copy 110.conf to prox1:/etc/pve/lxc/110.conf and 111.conf to prox2:/etc/pve/lxc/111.conf,
then on the respective host:

    pct create 110 local-zfs:vztmpl/debian-12-standard_amd64.tar.zst --rootfs local-zfs:subvol-110-disk-0,size=8G
    # (pct create with a pre-existing /etc/pve/lxc/<id>.conf is awkward; alternative:)
    pct create 110 local-zfs:vztmpl/debian-12-standard_amd64.tar.zst \
        --hostname prox-lxc1 --cores 2 --memory 2048 --swap 512 \
        --net0 name=eth0,bridge=vmbr0,ip=192.168.1.21/24,gw=192.168.1.1 \
        --unprivileged 1 --features nesting=1 --onboot 1 --startup order=2 \
        --rootfs local-zfs:subvol-110-disk-0,size=8G
    # then merge the idmap/mp/nameserver lines from 110.conf into /etc/pve/lxc/110.conf
    pct start 110

Same for 111 on prox2 (hostname prox-lxc2, ip 192.168.1.22, subvol-111-disk-0).

Storage pool mounts are added by the idmap/mp lines in the conf files:
  /usr/local/export      (library zfs pool, NFS group 2049 passthrough)
  /usr/local/recordings  (server1 recordings)

## Provision (inside each container)

Run tools/provision_sycon.sh as root on first boot.

## Topology

  prox-lxc1  192.168.1.21   (prox1, master)
  prox-lxc2  192.168.1.22   (prox2, replica)
  VIP     192.168.1.100   console.example.net  (keepalived, wildcard LE cert, TLS 443 -> nginx -> 127.0.0.1:8090)
  server1  192.168.1.10   Consul (LiteFS primary election) + nightly snapshot target

SQLite replication: LiteFS mounts /var/lib/syarc (contains syarc-console.db);
Consul on server1 arbitrates which node is primary. Litestream-style snapshots
of the LiteFS export go to server1 nightly for DR.

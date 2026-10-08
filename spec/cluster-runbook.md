# Cluster runbook

Operational procedures for a glidex cluster (spec/clustering.md §5, §12.5).
Commands run as `gxctl` on a server unless a step says it runs on the host.
Tokens are shown once and are read by the installer from a 0600 file or stdin,
never from the command line.

## Look first

    gxctl cluster status

Shows the Raft group (leader, voters, learners), every node's phase, version
and feature level, whether a CA rotation is under way, and any cluster port
(8842, and 6641-6644 with OVN) that a server cannot reach on another node.
UDP 6081 (Geneve) can't be probed this way.

## A node is down for a while

Nothing to do. It keeps its identity and comes back by itself (§5.4). Its VMs
show `Ready=Unknown/NodeUnreachable`; nothing is restarted elsewhere.

## Take a node out, empty

1. `gxctl node drain <node>`. Delete its VMs and disks, or detach it with them.
2. `gxctl node remove <node>`. Refused while it holds a VM, disk or node network,
   if it is the only gateway of the edge or of a VPC router, or if the Raft
   group would be left without a majority or with an even number of voters
   (`--force` for the even case only).
3. On that host, as root: `glidex-install --leave`. It refuses until step 2 has
   committed.

Removing a server rotates the CA afterwards (it held the key).

## A node is gone for good

1. Make sure it is powered off. `gxctl node forget <node> --fenced` (add
   `--force` if that leaves an even number of voters). Its VMs show
   `Ready=False/Lost`; its certificate is revoked; a forgotten server is rotated
   out of the CA.
2. When you no longer want its records: `gxctl node purge <node>`.

## A repaired host comes back as itself

1. `gxctl node rejoin-token <node>` (add `--raft-lost` if the host's
   `cluster/raft` directory is gone or older than its last vote: the node then
   returns as a learner with fresh state, never as the same voter).
2. On the host, as root: `glidex-install --rejoin <server>:8842 --token-file F`
   (add `--node-id <id>` if the host lost its cluster files).

A removed or departed node's id is retired: it joins as a new node instead.

## Rotating the CA

Automatic after a server leaves and a year before the CA expires. On demand:

    gxctl cluster rotate-ca [--grace <secs>]

Every node trusts old and new, renews its certificate, and the old CA retires
once all have renewed or the grace (default 7 days) is over. A node that was
unreachable the whole time rejoins with a token. `--grace 0` retires at the next
leader tick. Certificates also renew by themselves 30 days before they expire.

## Rolling upgrade

Upgrade agents, then servers one at a time (`gxctl cluster status` shows each
node's version and feature level). New write-set formats and schema migrations
are used only once every server reports the level; until then the cluster runs
on the lower one. Servers at N, agents at N or N-1.

## Lost quorum (a majority of servers destroyed)

1. On a surviving server, stop the control plane and take the latest snapshot
   (`gxctl cluster snapshot <file>` before the loss, or its local database).
2. `glidex-control-plane --force-new-cluster [--from <snapshot>]` starts a
   single-voter cluster from that state. Start the service.
3. Re-join the other servers with `--rejoin` and tokens issued with
   `--raft-lost`, so they start from fresh Raft state.
4. OVN's own groups need the same treatment: `ovsdb-tool cluster-to-standalone`
   on a surviving server's NB and SB files, then re-cluster the servers.
   (Not exercised by the test suite.)

// Prototype (scratch): the metadata endpoint delivered by tc/eBPF at a VM's
// port, with no path through OVS/OVN and no host route to the VM's subnet.
//
// md_in  (tc ingress on a VM's port): a request for 169.254.169.254:80 is
//        rewritten to come from the port's peer address 169.254.192.<idx>
//        and go to :8775, its VM's address and MAC are remembered, and it is
//        redirected into the host stack through md0's ingress.
// md_out (tc egress on md0): a reply to 169.254.192.<idx> is rewritten back
//        to the VM's address, from 169.254.169.254:80, and redirected out of
//        the VM's port.
// The port's index comes from the port the request arrived on (ifindex),
// never from the guest's addresses.

typedef unsigned char __u8;
typedef unsigned short __u16;
typedef unsigned int __u32;
typedef unsigned long long __u64;
typedef __u16 __be16;
typedef __u32 __be32;
typedef __u32 __wsum;

#include <linux/bpf.h>
#include <linux/if_ether.h>
#include <linux/ip.h>
#include <linux/in.h>
#include <linux/tcp.h>
#include <linux/pkt_cls.h>

#define SEC(x) __attribute__((section(x), used))
#define __uint(name, val) int (*name)[val]
#define __type(name, val) typeof(val) *name
#define LIBBPF_PIN_BY_NAME 1

static void *(*bpf_map_lookup_elem)(void *map, const void *key) = (void *)1;
static long (*bpf_map_update_elem)(void *map, const void *key, const void *value, __u64 flags) = (void *)2;
static long (*bpf_skb_store_bytes)(struct __sk_buff *skb, __u32 offset, const void *from, __u32 len, __u64 flags) = (void *)9;
static long (*bpf_l3_csum_replace)(struct __sk_buff *skb, __u32 offset, __u64 from, __u64 to, __u64 size) = (void *)10;
static long (*bpf_l4_csum_replace)(struct __sk_buff *skb, __u32 offset, __u64 from, __u64 to, __u64 flags) = (void *)11;
static long (*bpf_redirect)(__u32 ifindex, __u64 flags) = (void *)23;

#define MD_ADDR 0xa9fea9feu      /* 169.254.169.254 */
#define PEER_BASE 0xa9fec000u    /* 169.254.192.0 */
#define PEER_MASK 0xffffc000u    /* /18 */
#define GUEST_PORT 80
#ifndef LISTEN_PORT
#define LISTEN_PORT 8775
#endif

/* Port ifindex -> its index (set by netd when it attaches the port). */
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 16384);
    __type(key, __u32);
    __type(value, __u32);
    __uint(pinning, LIBBPF_PIN_BY_NAME);
} md_index SEC(".maps");

/* Index -> where its VM is (learned from its requests). */
struct peer {
    __u32 ifindex;
    __be32 vm_ip;
    __u8 vm_mac[6];
    __u8 pad[2];
};
struct {
    __uint(type, BPF_MAP_TYPE_HASH);
    __uint(max_entries, 16384);
    __type(key, __u32);
    __type(value, struct peer);
    __uint(pinning, LIBBPF_PIN_BY_NAME);
} md_peer SEC(".maps");

/* md0's ifindex and MAC, set once. */
struct md0 {
    __u32 ifindex;
    __u8 mac[6];
    __u8 pad[2];
};
struct {
    __uint(type, BPF_MAP_TYPE_ARRAY);
    __uint(max_entries, 1);
    __type(key, __u32);
    __type(value, struct md0);
    __uint(pinning, LIBBPF_PIN_BY_NAME);
} md_dev SEC(".maps");

#define IP_OFF ETH_HLEN
#define IP_CSUM (IP_OFF + __builtin_offsetof(struct iphdr, check))
#define IP_SRC (IP_OFF + __builtin_offsetof(struct iphdr, saddr))
#define IP_DST (IP_OFF + __builtin_offsetof(struct iphdr, daddr))
#define TCP_OFF (IP_OFF + sizeof(struct iphdr))
#define TCP_CSUM (TCP_OFF + __builtin_offsetof(struct tcphdr, check))
#define TCP_SPORT (TCP_OFF + __builtin_offsetof(struct tcphdr, source))
#define TCP_DPORT (TCP_OFF + __builtin_offsetof(struct tcphdr, dest))

static __always_inline int set_ip(struct __sk_buff *skb, __u32 off, __be32 from, __be32 to)
{
    if (bpf_l4_csum_replace(skb, TCP_CSUM, from, to, BPF_F_PSEUDO_HDR | 4))
        return -1;
    if (bpf_l3_csum_replace(skb, IP_CSUM, from, to, 4))
        return -1;
    return bpf_skb_store_bytes(skb, off, &to, 4, 0);
}

static __always_inline int set_port(struct __sk_buff *skb, __u32 off, __be16 from, __be16 to)
{
    if (bpf_l4_csum_replace(skb, TCP_CSUM, from, to, 2))
        return -1;
    return bpf_skb_store_bytes(skb, off, &to, 2, 0);
}

SEC("tc/ingress")
int md_in(struct __sk_buff *skb)
{
    void *data = (void *)(long)skb->data, *end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    struct iphdr *ip = data + IP_OFF;
    struct tcphdr *tcp = data + TCP_OFF;
    if ((void *)(tcp + 1) > end)
        return TC_ACT_OK;
    if (eth->h_proto != __builtin_bswap16(ETH_P_IP) || ip->ihl != 5 || ip->protocol != IPPROTO_TCP)
        return TC_ACT_OK;
    if (ip->daddr != __builtin_bswap32(MD_ADDR) || tcp->dest != __builtin_bswap16(GUEST_PORT))
        return TC_ACT_OK;
    __u32 ifindex = skb->ifindex;
    __u32 *idx = bpf_map_lookup_elem(&md_index, &ifindex);
    __u32 zero = 0;
    struct md0 *dev = bpf_map_lookup_elem(&md_dev, &zero);
    if (!idx || !dev || !dev->ifindex)
        return TC_ACT_SHOT; /* a port without an index: never let it through */
    struct peer p = { .ifindex = ifindex, .vm_ip = ip->saddr };
    __builtin_memcpy(p.vm_mac, eth->h_source, 6);
    __u32 key = *idx;
    bpf_map_update_elem(&md_peer, &key, &p, BPF_ANY);
    __be32 peer = __builtin_bswap32(PEER_BASE + key);
    __be32 vm_ip = ip->saddr;
    if (set_ip(skb, IP_SRC, vm_ip, peer) < 0)
        return TC_ACT_SHOT;
    if (set_port(skb, TCP_DPORT, __builtin_bswap16(GUEST_PORT), __builtin_bswap16(LISTEN_PORT)) < 0)
        return TC_ACT_SHOT;
    /* To md0, as if received there. */
    bpf_skb_store_bytes(skb, 0, dev->mac, 6, 0);
    return bpf_redirect(dev->ifindex, BPF_F_INGRESS);
}

SEC("tc/egress")
int md_out(struct __sk_buff *skb)
{
    void *data = (void *)(long)skb->data, *end = (void *)(long)skb->data_end;
    struct ethhdr *eth = data;
    struct iphdr *ip = data + IP_OFF;
    struct tcphdr *tcp = data + TCP_OFF;
    if ((void *)(tcp + 1) > end)
        return TC_ACT_SHOT;
    if (eth->h_proto != __builtin_bswap16(ETH_P_IP) || ip->ihl != 5 || ip->protocol != IPPROTO_TCP)
        return TC_ACT_SHOT;
    __u32 dst = __builtin_bswap32(ip->daddr);
    if ((dst & PEER_MASK) != PEER_BASE || tcp->source != __builtin_bswap16(LISTEN_PORT))
        return TC_ACT_SHOT;
    __u32 key = dst - PEER_BASE;
    struct peer *p = bpf_map_lookup_elem(&md_peer, &key);
    if (!p)
        return TC_ACT_SHOT;
    struct peer q = *p;
    if (set_ip(skb, IP_DST, ip->daddr, q.vm_ip) < 0)
        return TC_ACT_SHOT;
    if (set_port(skb, TCP_SPORT, __builtin_bswap16(LISTEN_PORT), __builtin_bswap16(GUEST_PORT)) < 0)
        return TC_ACT_SHOT;
    /* To the VM: its MAC; out of its port. */
    bpf_skb_store_bytes(skb, 0, q.vm_mac, 6, 0);
    return bpf_redirect(q.ifindex, 0);
}

char _license[] SEC("license") = "GPL";

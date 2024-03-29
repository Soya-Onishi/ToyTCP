#!/bin/bash

set -eux

# 名前空間の追加
ip netns add host1
ip netns add router
ip netns add host2

ip link add name host1-veth1 type veth peer name router-veth1
ip link add name host2-veth1 type veth peer name router-veth2

# 作成したlinkを名前空間に登録?
ip link set host1-veth1 netns host1
ip link set router-veth1 netns router
ip link set router-veth2 netns router
ip link set host2-veth1 netns host2

# 各linkに対してipアドレスを設定?
ip netns exec host1 ip addr add 10.0.0.1/24 dev host1-veth1
ip netns exec router ip addr add 10.0.0.254/24 dev router-veth1
ip netns exec router ip addr add 10.0.1.254/24 dev router-veth2
ip netns exec host2 ip addr add 10.0.1.1/24 dev host2-veth1

# 各linkをアクティベート?
ip netns exec host1 ip link set host1-veth1 up
ip netns exec router ip link set router-veth1 up
ip netns exec router ip link set router-veth2 up
ip netns exec host2 ip link set host2-veth1 up

# 各名前空間内のループバックをアクティベート?
ip netns exec host1 ip link set lo up
ip netns exec router ip link set lo up
ip netns exec host2 ip link set lo up

# ルーティングテーブルの設定
ip netns exec host1 ip route add 0.0.0.0/0 via 10.0.0.254
ip netns exec host2 ip route add 0.0.0.0/0 via 10.0.1.254
# 別セグメントへのIPフォワーディングを有効にする
ip netns exec router sysctl -w net.ipv4.ip_forward=1

# RSTフラグのパケットを無視するように設定?
ip netns exec host1 iptables -A OUTPUT -p tcp --tcp-flags RST RST -j DROP
ip netns exec host2 iptables -A OUTPUT -p tcp --tcp-flags RST RST -j DROP

# ???
ip netns exec host2 ethtool -K host2-veth1 tx off
ip netns exec host1 ethtool -K host1-veth1 tx off



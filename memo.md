# 自作TCPを通してのメモ

## `ip`コマンド周り

### `ip netns` 

ネットワーク内の名前空間の設定を行う。

#### `ip netns add`

名前空間を追加する。`lo`があらかじめ設定されている？

#### `ip netns exec`

`ip netns exec <名前空間> <その他コマンド>`とすることで、その名前空間の中で設定を行うことができる。
例えば`ip netns exec host1 ip addr add 10.0.0.1/24 dev host1-veth1`とすることで、
`host1`名前空間内の`host1-veth1`インタフェースにIPアドレス`10.0.0.1/24`を設定する操作ができる。

### `ip link`

ネットワークのデバイス（インタフェース？）の設定を行う。

#### `ip link add`

仮想的なデバイスの追加を行う。今回は`type`に`veth`を指定している。
`veth`を指定している場合、`ip link add name <device name> type veth peer name <pair device name>`とコマンドを打つことで
ペアを持つ`veth`のデバイスを作成することができる。

#### `ip link set`

`ip link set <device name> netns <namespace>`と記載した場合は、指定したデバイスを指定した名前空間に移動させる。
Manページから`lo`のような特別なデバイスは移動することができないらしい。

`ip link set <device name> up`とすることでそのデバイスを利用することができるようになる。
逆に言えば`up`していなければ、IPアドレスなどが設定されていてもpingなどでアクセスを試みた場合に`unreachable`になる。

## `sysctl`コマンド

### `sysctl -w net.ipv4.ip_forward=1`

本来NICをまたいだ通信というのはできない。
`net.ipv4.ip_forward=1`と設定することでNIC間をまたいだ通信を行うことができるようになる。
これは`sysctl`からだけでなく、`/etc/sysctl.conf`に`net.ipv4.ip_forward=1`を追加することでも同様の効果を得られる。


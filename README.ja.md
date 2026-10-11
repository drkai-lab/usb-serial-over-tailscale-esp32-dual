# usb-serial-over-tailscale(ESP32-WROOM-32 2 枚構成)

[English](README.md)

これは Tasuku Suzuki さんの [signal-slot/usb-serial-over-tailscale](https://github.com/signal-slot/usb-serial-over-tailscale) のフォークです。
本家は PSRAM 付きの ESP32-S3 1 枚を Tailscale のノードにして、対象ボードのシリアルコンソールを tailnet 上の TCP ポートに出すものです。
経緯は作者の記事 [ESP32-S3 単体を Tailscale ノードにして、SBC のシリアルコンソールをどこからでも開く](https://zenn.dev/tasuku/articles/0caef1a822c4e6)(Zenn)にあります。

手元に古い ESP32-WROOM-32 DevKitC が 2 枚余っていたので、同じことをこれでやりたいと思いました。
WROOM-32 には PSRAM がなく、制御クライアント・WireGuard・DERP・TCP スタックまで含めたノード一式を 1 枚に余裕をもって載せるのは無理でした。
そこでノードを 2 つのチップに分けました。片方は制御サーバとだけ話し、もう片方がパケットとシリアルコンソールを受け持ちます。
2 枚の間は UART でつなぎます。

## 現状

最初に読んでください。

- 2 つのファームウェア(役割)は `xtensa-esp32-espidf`(ESP-IDF v5.3.3、PSRAM なし、4 MB フラッシュ)向けにビルドできます。
- ホスト上のテストは通っています。2 つの役割を 1 プロセスで動かし、取りこぼしや破損を混ぜたソケットペアでつなぐ統合テストも含みます。
- **実機ではまだ動かしていません。** 2 Mbit/s の UART リンク、2 枚での Wi-Fi、メモリの余裕は、どれも未確認です。
- 下に書いたヒープとスタックの数字は、ビルド結果と逆アセンブルからの見積りで、実測ではありません。

今すぐ使えるものが必要なら、本家の ESP32-S3 版を使ってください。

## 分け方

| | チップ B(制御、`role-control`) | チップ A(データ、`role-data`) |
| --- | --- | --- |
| ネットワーク | Wi-Fi。制御サーバとだけ通信 | Wi-Fi。tailnet の通信はすべてこちら |
| Tailscale | ts2021 制御クライアント(Noise、HTTP/2)、マップストリーム、登録 | WireGuard、disco/STUN、DERP、パケットフィルタ(ACL)、smoltcp の TCP |
| 鍵 | マシン鍵 | ノード鍵(秘密鍵は A から出ない) |
| シリアル | UART0(USB)でセットアップ用シェルとログ | UART2 に対象のコンソール、UART0 にログ |

B は制御サーバからネットワークマップを受け取り、A 用のコンパクトな設定(アドレス、DERP マップ、ピア、パケットフィルタ)に変換して送ります。
A はそれを適用し、公開鍵、エンドポイント、ホーム DERP リージョン、ピアの状態を B に返します。
B が止まっても A は最後に適用した設定のまま動き続けるので、張られている接続は切れません。

チップ間のリンク(`crates/splitlink`):

- UART1、2 Mbit/s、RTS/CTS のハードウェアフロー制御。
- フレームは COBS で区切り、CRC-32 を付けます。メッセージごとに ACK を待ち、返ってこなければ再送します。
- 起動ごとに乱数の epoch を持つので、どちらかが再起動するとすぐに分かります。
- 設定には世代番号があります。まとまった変更があったときや A の世代が B と食い違ったときは全量を、それ以外はピアの追加・削除・更新だけの差分を送ります。

詳しくは [docs/split-design.md](docs/split-design.md)(英語)にあります。

## 用意するもの

- ESP32-WROOM-32 DevKitC(または USB シリアル変換チップ付きの互換ボード)2 枚。フラッシュ 4 MB、PSRAM は不要です。
- リンクと GND 用のジャンパ線 7 本(20 cm 未満)。
- 対象につなぐジャンパ線 3 本(TX、RX、GND)。
- USB ケーブル 2 本と、それぞれのボードの USB 電源。
- 対象は 3.3 V の UART であること。5 V や RS-232 の場合はレベル変換が要ります。

## 配線

![ESP32-WROOM-32 2 枚と対象の配線図](docs/wiring.png)

2 枚とも同じピン番号を使い、リンクの線はたすき掛けにします。

| 接続元 | 接続先 | 信号 |
| --- | --- | --- |
| B GPIO25 (TX) | A GPIO26 (RX) | リンク B→A |
| A GPIO25 (TX) | B GPIO26 (RX) | リンク A→B |
| B GPIO33 (RTS) | A GPIO32 (CTS) | フロー制御 |
| A GPIO33 (RTS) | B GPIO32 (CTS) | フロー制御 |
| B GND | A GND | GND 共通(必須) |
| A GPIO17 (TX) | 対象の RX | 対象のコンソール |
| A GPIO16 (RX) | 対象の TX | 対象のコンソール |
| A GND | 対象の GND | GND 共通(必須) |

- 電源はそれぞれの USB から取ります。ボード同士の 3V3 や 5V はつながないでください。
- B の USB は PC につないでセットアップ用シェルに使います。B の BOOT ボタン(GPIO0)を押したまま電源を入れるとセットアップモードになります。
- 避けたピン: GPIO6〜11(フラッシュに接続)と、ストラップピンの 2、5、12、15。GPIO32 と 33 は、DevKitC には 32 kHz 水晶がないので空いています。
- リンクの線は短くしてください。長くするなら、両方のボードで `firmware/src/link_uart.rs` の `LINK_BAUD` を下げます。

図の元データは [docs/wiring.svg](docs/wiring.svg) です。

## ビルド

`espup` で入れる `esp` ツールチェーンと、`ldproxy`、`espflash` が必要です。
ESP-IDF 本体は最初のビルドで `firmware/.embuild/` にダウンロードされます。時間がかかり、ディスクも数 GB 使います。

```bash
cargo install espup ldproxy espflash
espup install
```

`~/export-esp.sh` は source しないでください。ビルドは esp-idf-sys が `.embuild/` に入れる gcc を使います。espup の Xtensa gcc が `PATH` の先頭にあるとリンクに失敗します。

役割ごとにターゲットディレクトリを分けてビルドします(同じディレクトリだと上書きされます)。

```bash
cd firmware
CARGO_TARGET_DIR=target-control cargo build --release --features role-control
CARGO_TARGET_DIR=target-data    cargo build --release --features role-data
```

手元のビルドでのイメージサイズは、B が約 1.44 MB、A が約 1.33 MB でした。

ホスト上のテスト(リポジトリのルートで):

```bash
cargo test --workspace
cargo test -p tsnode --features split          # 2 つの役割を取りこぼしのあるソケットペアでつなぐ
cargo test -p tsnode --features lowmem,split
```

## 書き込み

WROOM-32 のフラッシュは 4 MB です。`partitions.csv`(大きい S3 モジュール向けの 6 MB アプリ領域)ではなく、`partitions-wroom32.csv` を使ってください。
DevKitC の USB シリアルは Linux ではたいてい `/dev/ttyUSB0` です。環境に合わせて変えてください。

```bash
cd firmware
# チップ B
espflash flash --chip esp32 --port /dev/ttyUSB0 --flash-size 4mb \
  --partition-table partitions-wroom32.csv \
  target-control/xtensa-esp32-espidf/release/usb-serial-over-tailscale-firmware
# チップ A(差し替えるか、A のポートを指定)
espflash flash --chip esp32 --port /dev/ttyUSB0 --flash-size 4mb \
  --partition-table partitions-wroom32.csv \
  target-data/xtensa-esp32-espidf/release/usb-serial-over-tailscale-firmware
```

`cargo run` は `firmware/.cargo/config.toml` の runner を使いますが、これは S3 ボード向け(16 MB、`/dev/ttyACM0`)のままです。上のコマンドを使ってください。

## セットアップ

1. 2 枚に書き込み、上のとおり配線します。
2. B のシリアルポートを 115200 bps で開き(`screen /dev/ttyUSB0 115200`)、Enter を押します。ウィザードは本家と同じで、Wi-Fi、ホスト名のあと、Tailscale の承認 URL が出ます(auth key を使うなら `authkey tskey-auth-...` のあと `login`)。
3. B は Wi-Fi のネットワーク、TCP ポート、ボーレートを A に転送します。A はそれを保存し、ブリッジを始めるために一度再起動します。
4. B は A が報告したノード公開鍵で登録し、以後 A の設定を最新に保ちます。
5. tailnet 上の別のマシンから、生の TCP クライアントでつなぎます。本家の記事では socat を使っています。

   ```bash
   socat -,rawer,escape=0x1d TCP:target-console:35930   # Ctrl-] で抜ける
   ```

B の `status` で、Tailscale の状態と、データチップにつながっていて設定が同期しているかが見られます。`port <n>` と `baud <rate>` で TCP ポート(既定 35930)と対象のボーレート(既定 115200)を変えられます。

## メモリの見積り

実測ではなく見積りです。

| | B(制御) | A(データ) |
| --- | --- | --- |
| 静的 DRAM(.data + .bss) | 31.5 KB | 31.5 KB |
| Wi-Fi 起動後のヒープ(仮定) | 180〜220 KB | 180〜220 KB |
| 定常の使用量(スタック+ヒープ) | 約 95 KB | 約 130 KB |
| ピーク | 150〜170 KB(マップのデコード) | 約 170 KB(DERP の TLS ハンドシェイク+全量の適用) |

厳しいのは A のほうです。実機で最初に測るのはここです。

## 制限

- 実機では未確認です(「現状」を参照)。
- ピアは最大 32、DERP の同時接続は 2 まで。内部 RAM に収めるための `lowmem` ビルドの制限です。
- メモリ節約のため lwIP の IPv6 を切っています。tailnet の IPv6 アドレスは設定されますが、インターネットへは IPv4 だけで出ます。
- シリアルは 1 本(UART2)だけです。本家 S3 版のように USB インターフェースごとのポートにはなりません。
- Wi-Fi のパスワードは、チップ間のリンクを平文で流れます。自分の 2 枚の間の短い線ですが、気にはしておいてください。
- A は設定を RAM にしか持っていません。B が止まっている間に A が再起動すると、B が戻るまで A にはピアがありません。
- B で見えるピアの状態は 2 秒ごとの更新です。
- 2 枚の間のリセット配線やウォッチドッグはまだありません。
- `web/` のブラウザ書き込みツールと `.github/` の CI は本家のもので、S3 版向けです。

## 今後

フェーズ 2 は実機での立ち上げです。

1. 実際の配線で RTS/CTS 付き 2 Mbit/s の UART リンクを確かめ、ACK のタイムアウトを調整する。
2. 空きヒープ、最大連続ブロック、各スレッドのスタック残量を測り、上の見積りを置き換える。
3. 実際の tailnet で端から端まで試す。登録、DERP 経由と直接経路、コンソールのブリッジ、セッション中に B を止めたときの挙動。
4. A の最後の設定を NVS に保存し、B が止まっている間の再起動でも残るようにする。
5. EN ピンのクロス配線か、チップ間のウォッチドッグを入れる。

## ブランチ

- `esp32-split`(既定): ここで説明している 2 枚構成。
- `esp32-nopsram`: WROOM-32 1 枚に全部載せようとした版。ビルドはできますが、見積りではピーク時のメモリが足りません。
- `esp32-uart-probe`: 最初の実験。ESP32 1 枚(M5StickC Plus2)で、USB ホストの代わりに UART を使う版。
- `main`: 本家のまま。

## クレジットとライセンス

元の設計と、Tailscale 実装のほぼすべて(制御クライアント、WireGuard、DERP、disco、netstack)は、Tasuku Suzuki さんの [signal-slot/usb-serial-over-tailscale](https://github.com/signal-slot/usb-serial-over-tailscale) によるものです。[Zenn の記事](https://zenn.dev/tasuku/articles/0caef1a822c4e6)もあわせてどうぞ。
このフォークで足したのは、WROOM-32 への移植、制御とデータの分割、チップ間リンクのプロトコルです。

本家の README は [docs/UPSTREAM_README-ja.md](docs/UPSTREAM_README-ja.md)([English](docs/UPSTREAM_README.md))に残してあります。

ライセンスは本家と同じ MIT です。[LICENSE](LICENSE) を見てください(Copyright (c) 2026 Tasuku Suzuki)。

# Logicool G (PRO Wireless) — HID++ 2.0 プロトコル解析と G HUB の置き換え

Device HUB で Logicool G HUB を置き換えるためのメモ。対象は **PRO Wireless**
(有線 `046d:c088` / LIGHTSPEED 受信機 `046d:c539`)。同じ HID++ 2.0 +
オンボードプロファイル (0x8100) を持つマウスなら同じドライバで動く想定。

- ドライバ: `crates/devhub-hid/src/logitech.rs` (`DriverKind::Logitech`)
- 解析ツール: `cargo run -p devhub-hid --example logiprobe` (読み取り専用)
- 常駐: `crates/device-hub/src/autostart.rs`
- G HUB 削除: `scripts/remove-ghub.ps1` / 一括導入: `scripts/install-ghub-replacement.ps1`

> **状態: G703 LIGHTSPEED (有線 c090 / 受信機 c539) で DPI・ポーリング・ボタン・LED の読み書きを確認済み (2026-11-18)。PRO Wireless 本体は未確認。**
> 公開されている HID++ 2.0 仕様と、libratbag / Solaar の
> オンボードプロファイル実装を元に組んだもの。実機で `logiprobe` の出力を取り、
> 下の「実機で確かめること」を埋めてから registry の `verified` を true にする。

---

## 1. 接続とインターフェース

| 接続 | VID:PID | device index | 設定 IF |
|---|---|---|---|
| USB ケーブル (有線) | 046d:c088 | `0xFF` | FF00:0002 (long) |
| LIGHTSPEED 受信機 | 046d:c539 | `0x01` (ペア枠 1) | FF00:0002 (long) |
| (参考) PRO X SUPERLIGHT 受信機 | 046d:c547 | `0x01` | FF00:0002 |

Windows の hidapi では vendor collection が 2 つの別デバイスとして列挙される:

- `FF00:0001` … short report `0x10` (7B)。受信機の HID++ 1.0 通知 (接続 0x41・エラー 0x8F)。
- `FF00:0002` … long report `0x11` (20B)。HID++ 2.0 の要求/応答はすべてこちら。

ドライバは long を設定チャネルとして開き、受信機の場合だけ short を別スレッドで開いて
接続通知を監視する (`spawn_link_watch`)。書き込みは必ず 20B ちょうど。

## 2. フレーム

```
要求/応答  [0x11, dev, featIdx, func<<4 | swId, params × 16]
エラー 2.0 [0x11, dev, 0xFF, featIdx, func<<4 | swId, code, …]
エラー 1.0 [0x10, dev, 0x8F, featIdx, func<<4 | swId, code, 0]   ← 受信機 (マウス不在など)
通知       [0x11, dev, featIdx, event<<4 | 0, …]                 ← swId = 0
```

- swId はこちらで選ぶ 1..15 (ドライバは `0x0B`)。応答の照合は dev / featIdx / func / swId の全一致。
- 2.0 エラーコード: 1 Unknown, 2 InvalidArgument, 3 OutOfRange, 4 HWError,
  5 Internal, 6 InvalidFeatureIndex, 7 InvalidFunctionId, 8 Busy, 9 Unsupported。
- feature は **ID → index** を Root (index 0) の func0 `getFeature(idHi, idLo)` で引く
  (`r[0]` = index、0 なら非対応)。index は機種・FW で変わるので毎回引く (キャッシュは接続中のみ)。
- Root func1 `ping(0, 0, x)` → `[protoMajor, protoMinor, x]`。無線で応答が無い = スリープ/圏外。

## 3. 使う feature

| ID | 名前 | 使う関数 | 用途 |
|---|---|---|---|
| 0x0000 | Root | 0 getFeature / 1 ping | index 解決・生存確認 |
| 0x0001 | FeatureSet | 0 count / 1 getFeatureId(i) | 解析ツールの一覧表示 |
| 0x0003 | DeviceFwVersion | 0 count / 1 getFwInfo(e) | FW 表示 `"XXX 12.01.B0019"` (type 0 = アプリ) |
| 0x0005 | DeviceName | 0 length / 1 getName(off) | 表示名 (受信機経由でも実機名) |
| 0x1000 / 0x1001 / 0x1004 | 電池 | 0 / 0 / 1 | 残量・充電。PRO Wireless は 0x1001 (電圧) |
| 0x2201 | AdjustableDpi | 1 getSensorDpiList(0) | DPI の範囲と刻み (`0xE000\|step` = 範囲指定) |
| 0x8060 | ReportRate | 0 getReportRateList | 対応ポーリング (bit n = (n+1) ms) |
| 0x8100 | OnboardProfiles | 下記 | 設定の読み書き本体 |

電池 0x1001: `[mV hi, mV lo, flags]`、flags bit7 = 外部電源、`flags & 7 == 2` = 満充電。
残量 % は Solaar と同じ電圧→% 近似表で線形補間 (`voltage_to_percent`)。

## 4. オンボードプロファイル (0x8100)

G HUB の「オンボードメモリ」機能そのもの。マウス内フラッシュのプロファイルを書き換えるので、
**設定はマウス本体に残り、ソフトが動いていなくても / 別 PC でも効く**。

| func | 名前 | 引数 → 応答 |
|---|---|---|
| 0 | getDescription | → `[memModel, profileFmt, macroFmt, profileCount, oobCount, buttonCount, sectorCount, sectorSize(BE16), mechLayout, various]` |
| 1 | setMode | `[1]` オンボード / `[2]` ホスト (G HUB 起動中はホストにされていることがある) |
| 2 | getMode | → `[mode]` |
| 3 | setCurrentProfile | `[0, n]` (n = 1 始まり) — 同じ番号を再設定すると書き換え後の内容を再読込する |
| 4 | getCurrentProfile | → `[hi, lo]` (ユーザー n / ROM 0x0100+n) |
| 5 | memoryRead | `[sectorHi, sectorLo, offHi, offLo]` → 16B (off は sectorSize-16 まで) |
| 6 | memoryAddrWrite | `[sectorHi, sectorLo, 0, 0, lenHi, lenLo]` |
| 7 | memoryWrite | 16B ずつ |
| 8 | memoryWriteEnd | — |
| 0xB / 0xC | get/setCurrentDpiIndex | (解析ツールで表示のみ) |

### セクタ

- `0x0000` … ディレクトリ。4B エントリ `[sectorHi, sectorLo, enabled, 0]` × profileCount、`0xFFFF` で終端。
- `0x0001..` … ユーザープロファイル (n 番目 = n)。
- `0x0101..` … ROM (工場出荷) プロファイル。読み取り専用。
- 各セクタの末尾 2B = CRC-CCITT (init 0xFFFF, poly 0x1021) を **BE** で。CRC が合わないセクタは未使用扱い。

### プロファイルのレイアウト (profileFormat 1..5 共通部分)

| off | 長さ | 内容 |
|---|---|---|
| 0 | 1 | ポーリング = `1000 / Hz` (1=1000Hz, 2=500, 4=250, 8=125) |
| 1 | 1 | 起動時の DPI ステージ index |
| 2 | 1 | DPI シフト (スナイパー) で使うステージ index |
| 3 | 2×5 | DPI ステージ (LE16)。0 / 0xFFFF は未使用 |
| 13 | 3 | プロファイル色 |
| 16 | 1 | power mode |
| 17 | 1 | angle snapping |
| 28 / 30 | 2 / 2 | 省電力 / 電源 OFF までの時間 |
| 32 | 4×16 | ボタン割り当て |
| 96 | 4×16 | G-Shift 側の割り当て |
| 160 | 48 | プロファイル名 (UTF-16LE) |
| 208 | 11×2 | LED (ゾーン 0, 1) |
| 230 | 11×2 | 省電力時の LED |
| size-2 | 2 | CRC |

### ボタン割り当て 4B

| byte0 | byte1 | byte2..3 | 意味 |
|---|---|---|---|
| 0x80 | 0x00 | — | 無効 |
| 0x80 | 0x01 | ボタンビット (16bit, 1<<n) | マウスボタン (n=0 左, 1 右, 2 中, 3 戻る, 4 進む) |
| 0x80 | 0x02 | modifier, HID usage | キーボード |
| 0x80 | 0x03 | Consumer usage (BE16) | メディア |
| 0x90 | special | — | 1 左チルト, 2 右チルト, 3 DPI+, 4 DPI−, 5 DPI 順送り, 7 DPI シフト, 10 プロファイル順送り |
| 0x00 | page | offset | マクロ (未対応 — 生のまま往復保存) |
| 0xFF | — | — | 未設定 |

ボタンビットのエンディアンは実装間で揺れがあるため、ドライバは **左ボタン (slot 0) の値で自動判定**する
(`00 01` なら BE、`01 00` なら LE)。

### LED 11B

| mode | byte1..3 | 以降 |
|---|---|---|
| 0x00 消灯 | | |
| 0x01 固定 | RGB | |
| 0x03 サイクル | — | [6..8] 周期 ms (BE), [8] 明るさ |
| 0x0A 呼吸 | RGB | [4..6] 周期 ms (BE), [6] 波形, [7] 明るさ |

## 5. 書き込みの流れ (ドライバ `commit`)

1. ディレクトリと現在プロファイルを読む。ディレクトリの CRC が壊れている (= G HUB でも一度も
   書いていない) なら ROM プロファイルで動いている扱い。
2. 変更を当てたプロファイルを **ユーザーセクタ n** に書き、読み戻して一致を確認。
3. ROM を指していた他のプロファイルは、先に ROM の中身をユーザーセクタへ写す
   (ディレクトリが空セクタを指す事故を防ぐ)。
4. ディレクトリを書き直す (変化があるときだけ)。
5. `setCurrentProfile(n)` で再読込 → 即反映。

設定変更 1 回 = セクタ 1 枚の書き込み。DPI スライダーのドラッグ中は UI 側で古い変更が
間引かれるので、書き込み回数は離した位置までの数回で済む。

起動時に `getMode` がホスト (2) ならオンボード (1) に戻す。G HUB がホストモードのまま
残した状態でも、DPI ボタン等がマウス単体で効くようにするため。

## 6. Device HUB で出来ること / 出来ないこと

| 項目 | 状態 |
|---|---|
| DPI ステージ (1〜5、範囲は 0x2201 から) / 起動時ステージ | ○ |
| ポーリング 125/250/500/1000 | ○ |
| ボタン: マウス・チルト・DPI±/順送り/シフト・キー・メディア・プロファイル順送り・無効 | ○ |
| LED: 消灯/固定/呼吸/サイクル (ゾーン 0) | ○ |
| オンボードプロファイル切替・名前 | ○ |
| 電池 (0x1000/0x1001/0x1004)・FW・名前 | ○ |
| プロファイルを出荷時に戻す | ○ |
| マクロ (macroFormat) | × (既存マクロは壊さず保持) |
| G-Shift 側の割り当て | × |
| ファーム更新 | × (G HUB で行う) |

## 7. 実機で確かめること

`cargo run -p devhub-hid --example logiprobe` の出力を下に貼って埋める。

- [x] usage page/usage が `FF00:0002` であること (G703 c090)
- [x] getDescription の sectorSize / profileFormat / buttonCount (G703)
- [ ] getCurrentProfile の値 (ROM で動いているとき 0x0101 か)
- [x] ボタンビットのエンディアン (`bits_le`) — BE (G703)
- [ ] LED ゾーン 0 がロゴか
- [x] DPI・ポーリングの書き込み、ケーブル抜き差し後の保持 (G703 有線)
- [x] ボタン・LED の書き込み (G703、LED は 0x8071 を本体制御へ戻し 2 ゾーンへ書いて反映)
- [x] LIGHTSPEED 受信機 (c539) 経由の読み書き (G703)
- [ ] 受信機の short collection の 0x41 通知でスリープ/復帰が取れるか

G HUB の挙動を観察したいときは、G HUB を起動したまま `logiprobe -- sniff` を実行すると
応答と通知が時刻付きで流れる (他プロセスの送信内容は Windows では見えないが、
応答の featIdx/func から要求はほぼ復元できる)。

### 実機ログ

**G703 LIGHTSPEED (HERO) 有線 046d:c090** (2026-11-18、読み取りのみ):

- HID++ 4.2、device index 0xFF。IF2 に FF00:0001 / FF00:0002。
- features: 0x0001 0x0003 0x0005 0x1D4B 0x0020 **0x1001** 0x8071 **0x8100** 0x8110 **0x8060** **0x2201** …
- FW: `MPM 22.02.B0015` (type 0) / `BL1 05.02.B0015` (bootloader)。
- 0x2201 list `00 64 E0 32 64 00` → 100–25600 / 50。0x8060 mask `0b10001011` → 125/250/500/1000。
- 0x8100 desc `01 04 01 05 01 06 10 00 FF 0A 04` → **format 4, profiles 5, buttons 6, sector 255B**、mode=1 (onboard)。
- getCurrentProfile = 0x0001 (ユーザー 1)。ディレクトリ有効 (1 のみ enabled)。
- プロファイル CRC は本実装の CRC-CCITT と一致 (`fixtures/logitech/g703-wired-profile1.txt`, テスト `g703_real_profile`)。
- ボタンビットは **BE** (`80 01 00 01` = 左)。slot 5 = `90 05` (DPI 順送り)。
- LED は 2 ゾーンとも `03 … 1F 40 64` = サイクル 8000ms / 明るさ 100 — 本書のレイアウト通り。
- 起動時 DPI 枠 (off 1) = 1 と、現在の DPI 枠 (0x8100 func 0xB) = 2 は別物 → 表示は現在枠を優先。

---

## 8. 常駐と G HUB の削除

- アプリ設定 → 「常駐 (G HUB の代わり)」
  - **Windows 起動時に自動起動**: `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\DeviceHub`
    に `"<exe>" --background` (最小化で起動)。
  - **閉じても常駐** (既定 ON): × で終了せず最小化。プロセス連動プロファイル・電池監視を続ける。
- 多重起動は名前付き Mutex で防止し、2 個目は既存ウィンドウを前面に出して終了する。
- `scripts/install-ghub-replacement.ps1` … ビルド → `%LOCALAPPDATA%\Programs\DeviceHub` へ配置 →
  自動起動登録 → スタートメニュー → 起動。`-RemoveGHub` で確認後に G HUB 削除まで。
- `scripts/remove-ghub.ps1` (管理者) … プロセス/サービス停止 → 設定を Desktop に ZIP バックアップ →
  `lghub_updater.exe --uninstall` → サービス・Run キー・タスク・フォルダの掃除。
  `-RemoveDrivers` で G HUB 仮想デバイスドライバ (`logi_joy_*.inf`) も削除。`-WhatIf` で予行演習。

G HUB を消してもマウスの設定はオンボードメモリに残るので、消した直後から元の設定のまま動く。
## Logicool LIGHTSPEED

The G913/G915 TKL receiver (046D:C545, HID++ long collection FF00:0002)
exposes keyboard brightness, report rate, battery voltage and two RGB clusters.
This driver reads the paired keyboard name before offering those controls.
Fixed-colour, five-second colour-cycle and off requests use the host lighting mode. The colour shown
after a request is the last colour sent in the current session.
Brightness, report rate and battery are read directly from the keyboard.

The mouse backend reads the onboard profile. Opening a receiver leaves its
mode unchanged. Select the onboard-mode switch in General before editing
mouse settings. G HUB application profiles and G-Shift tables are separate
from the active onboard profile.

For a hardware check, run
`cargo run --locked -p devhub-hid --example lightspeed_check -- --brightness-roundtrip`.
It changes the keyboard brightness, checks the reply, restores the previous
value and checks again. Mouse settings are only read by this command.

For G502 X LIGHTSPEED identified by device name and onboard format 3 with
11 buttons, the button page also exposes the 11 G-Shift bindings. The wheel
scroll and reserved fields in each 16-entry table are left intact. Both
layers use the G HUB slot order. The mouse has no lighting controls.

Use `--mouse "G502 X"` and/or `--mouse "G703"` with the hardware check to
require each named mouse to return settings. A disconnected receiver alone
cannot satisfy the check.

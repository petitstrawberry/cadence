# Cadence

ScarletUI 製の GUI 音楽プレイヤー。Scarlet を主対象に、同じ再生コアと画面を
macOS / Linux でも使えるようにした、独立した Rust workspace です。
Resonara のプロジェクト構成を参考にしています。

## 起動

Nix を用意し、このリポジトリで実行します。

```sh
./scripts/dev cargo run --locked --release -p cadence
./scripts/dev cargo run --locked --release -p cadence -- /path/to/song.flac /path/to/another.mp3
```

引数のファイルはキューへ追加します。再生開始には **Play** を押してください。
`scripts/dev` は固定した Scarlet Rust / Nixpkgs の開発環境を使います。
別の Nix 実行ファイルを使う場合は `CADENCE_NIX=/path/to/nix-wrapper` を指定できます。
`CARGO_HOME` と `CARGO_TARGET_DIR` は通常通り変更できます。

macOS で通常のアプリとして開く場合は、ローカル用の `.app` を作成できます。

```sh
./scripts/dev scripts/bundle-macos
open target/macos/Cadence.app
```

バンドルは既定で release ビルドです。デバッグ用には `--debug` を指定できます。

出力先は
`${CARGO_TARGET_DIR:-target}/macos/Cadence.app` です。署名・公証を含む配布用パッケージではありません。

依存する ScarletUI は `86a765daaf26545fc0294aa7054d98630df9e368`、Scarlet の
SAS / runtime は `b3d2a55740a3d2ca49daad0ec7baba233f706f7a` に Git pin しています。
推移的な SGFX の解決結果は `Cargo.lock` に記録しています。
隣接 checkout は通常ビルドに不要です。SWS protocol の更新だけは
[`vendor/sws-protocol`](vendor/sws-protocol/README.md) の portable patch を使います。

## 操作

- Kanade App の Mac 版に合わせ、左にナビゲーション、中央にコレクション、右にキュー、
  中央下部に小さな再生バーを配置しています。狭いウィンドウでは右キューを閉じ、**Play queue** から開けます。
- **Albums** はカバーからアルバムを開き、**Artists** はアーティスト → アルバム → 曲へ進みます。
  **All songs** は検索できる曲一覧です。曲をクリックすると、その一覧・アルバムのキューで直ちに再生します。
  アルバム詳細ではアートワークと曲一覧が一緒にスクロールし、disc / track のタグ順で表示します。
- **Play queue** の行をクリックすると再生します。行末の × でその曲を外し、**Clear** で全体を消去します。
  **Add files…** はファイルの追加です。macOSでは標準のファイル選択シートを使い、非対応環境ではアプリ内ブラウザーを開きます。
- フローティングの再生バーで再生 / 一時停止、前曲 / 次曲、音量、シーク、リピートを操作できます。
  全ページで同じ位置・幅を使い、一覧の末尾がバーに隠れないようスクロールできる余白を確保しています。
- **Space** で再生 / 一時停止、**← / →** で 5 秒シーク、**Ctrl / Cmd + O** でファイルを追加します。
- **Sources** → **Add source** で Local folder / Kanade / DLNA / Source plugin を選びます。
  すべて同じソース一覧に並びます。Local folder はmacOSの標準フォルダー選択シートから登録し、サブフォルダーまで走査します。
  ソース内の **Refresh**、進捗とキャンセル、エラー詳細も同じ画面で扱い、独立したscanページはありません。
  ソースを削除すると、そのソースの曲をカタログからも除去します。音声ファイルと、他の登録フォルダーが共有する曲は残します。ソース内には一括再生・キュー追加・ライブラリー追加のボタンを置いていません。
  Kanade / DLNA / plugin の曲はダウンロードせず、再生時にサービスからストリーミングします。
- **Effects** を開くとOSごとの標準CLAPディレクトリをバックグラウンドで検索します。
  **Available** から選択して **Add**。ヘッダーの更新ボタンで再検索でき、個別の `.clap` ファイル／bundle を指定する **Discover** も使えます。
  チェーンの並べ替え・バイパス・削除と、汎用パラメーター操作ができます。設定とプラグイン状態も保存します。

Symphonia で WAV / AIFF / CAF の PCM、FLAC、MP3、AAC、ALAC、Ogg Vorbis などを読み取ります。
拡張子が対応していても、コンテナー内のコーデックによっては再生できません。
タイトル・アーティスト・アルバム・長さは再生時に読み取ります。
CAF の info / WAV の LIST INFO タグは現在のデコーダーでは取得できず、ファイル名を表示します。

## 構成と現在の範囲

`cadence-core` は OS 非依存の再生・デコード・キュー、`cadence-platform` は
Scarlet の SAS とホストの CPAL、`cadence-library` は永続カタログとアートワーク、
`cadence-sources` はサービス接続、`cadence-effects` は CLAP、`cadence` は ScarletUI の画面を担当します。
デコーダー、音声出力、音声処理、ソースは Rust trait で差し替えられます。
詳しくは [アーキテクチャ](docs/architecture.md) を参照してください。

CLAP ホストには Resonara の `resonara-clap` を
`7b17e9f87714d47f572526dbd239282ab4745ce3` で固定して使用しています。
現在は stereo / f32 のゼロレイテンシー音声エフェクトを対象とし、専用プラグイン GUI、
インストゥルメント、MIDI、レイテンシー補償、曲末尾のエフェクト tail 延長は未対応です。
各 OS / CPU 向けにビルドされた CLAP が必要です。Scarlet では Scarlet 向け ELF を使います。

検索先はResonaraと[CLAPの標準検索パス](https://github.com/free-audio/clap/blob/main/include/clap/entry.h)に沿っています。
macOSは `/Library/Audio/Plug-Ins/CLAP` と `~/Library/Audio/Plug-Ins/CLAP`、
Linux/Scarletは `/usr/lib/clap`、`/usr/local/lib/clap`、`~/.clap`、`~/.local/lib/clap`、
Windowsは `%COMMONPROGRAMFILES%\CLAP` と `%LOCALAPPDATA%\Programs\Common\CLAP`。
`CLAP_PATH` と実行ファイル隣の `plugins/` も検索し、重複・循環を除外します。
検索は最大16,384エントリ／1,024ライブラリー／深さ32に制限します。


キューはメモリー上のみです。プレイリスト保存とシャッフルは未実装です。
自然な曲送りとリピートは次曲を先読みし、同じ音声出力へPCMを連続して送ります。
曲間の無音は追加しません。ネットワークの供給不足による途切れや、元ファイル内の
無音は別です。圧縮音声のencoder delay / padding除去は
[Symphoniaの形式・コーデック対応](https://docs.rs/symphonia/0.6.1/symphonia/)に従い、
AAC／MP4などではサンプル単位の境界を保証できません。
入力はモノラル / ステレオに限定しています。リサンプルは線形補間で、
ダウンサンプリング時の折り返し雑音を抑えるフィルターはまだありません。

## ライブラリーとサービス

保存先は `CADENCE_DATA_DIR`、`$XDG_DATA_HOME/cadence`、`$HOME/.local/share/cadence` の順で選びます。
環境変数がない場合は `/home/cadence` です。`library.json` にカタログ、`app.json` にソースと
CLAP の設定、`cache/` に取得した画像や明示的なダウンロードのキャッシュを保存します。
ストリーミング再生では音源ファイルを作りません。読み込みに失敗した JSON は上書きしません。
PNG / JPEG のカバーに対応し、画像とカタログのサイズには上限を設けています。

- **Kanade:** サーバーのベース URL（例 `http://server:8080`）を入力します。
  WebSocket からカタログを取得し、音源・画像の署名 URL は利用時に取得します。
  再生・一時停止中も接続と heartbeat を維持し、署名 URL はカタログに保存しません。
- **DLNA:** MediaServer の device description URL を入力します。
  ContentDirectory のフォルダー・音声・カバーを扱い、音源 URL は再生時に BrowseMetadata で解決します。
  SSDP 自動検出とリモートスピーカー操作は未対応です。
- **Plugin:** 外部ソース実行ファイルの絶対パスを指定します。本体の再ビルドなしでサービスを追加できます。
  JSON Lines による [プロトコルと実装例](docs/source-plugin-protocol.md) を用意しています。
  引数や個別設定が必要なプラグインは、終了中に `app.json` の該当ソースの `settings` を編集します。
  現在はホスト OS で利用できます。Scarlet の子プロセス強制終了 API が未対応のため、
  Scarlet では起動を無効にしています。組み込み Kanade / DLNA はこの制約を受けません。

外部音源は必要なデータを読みながら再生します。HTTP Range に対応するサーバーではシークできます。
Range 非対応の音源も一時停止と再開ができ、バーは操作できない進行表示に切り替わります。
開始時に末尾の情報を要求するコンテナーでは、サーバーの Range 対応が必要になる場合があります。
Rust API の明示的なダウンロードは最大 512 MiB、キャッシュ全体は 2 GiB に制限します。
ホストでは HTTPS / WSS に対応します。現在の Scarlet 向け transport は HTTP / WS のみで、
HTTPS / WSS を指定すると明示的にエラーになります。

## 検証

```sh
# フォーマット、workspace テスト、ホストのビルド
./scripts/dev bash scripts/verify
# Scarlet AArch64 / RISC-V64 の release ビルドと ELF 検査
./scripts/dev bash scripts/verify-scarlet
# 片方だけ確認する場合
./scripts/dev bash scripts/verify-scarlet aarch64-unknown-scarlet
```

実 CLAP のテストには、固定 revision と同じ Resonara checkout の Gain を利用できます。
通常の Cadence ビルドでは隣接 checkout は不要ですが、以下の検証例では必要です。

```sh
./scripts/dev cargo build --release --manifest-path ../resonara/plugins/resonara-gain/Cargo.toml \
  --target-dir ../resonara/plugins/resonara-gain/target
./scripts/dev cargo test --locked -p cadence-effects --test native_gain -- --include-ignored
# Scarlet 向け CLAP のビルドと ELF 検査。RISC-V64 は --arch riscv64
./scripts/dev python3 ../resonara/plugins/resonara-gain/build.py \
  --arch aarch64 --output artifacts/clap-gain-aarch64
```

Scarlet 用 Gain は出力内の `staging/usr/lib/clap/resonara-gain.clap` と LICENSE を配置し、
Effects からそのパスを指定します。Gain は `no_std` の CLAP です。
他のプラグインは個別に Scarlet の実行形式・共有ライブラリー・TLS 制約へ対応させる必要があります。

ログと ELF 検査結果は `artifacts/` に保存します。Scarlet の固定ツールチェーンが同梱する
標準ライブラリーは PIC ではないため、`-Z build-std=std,panic_abort` でソースから再ビルドします。
さらに `-Z build-std-features=compiler-builtins-mem` でメモリー / 文字列の組み込み関数を含めます。
ソースも固定ツールチェーンに含まれます。依存と標準ライブラリーの両方に PIC を適用します。
`verify-scarlet` は `readelf` または `llvm-readelf` を使用します。

これらはビルド・単体テスト・ELF の検証コマンドです。GUI の表示と入力、SWS / SAS の応答、
実際の出音は別途確認します。Scarlet 実行時の検証はまだ完了していません。
Windows 等への移植を可能にする境界はありますが、対応済みとは扱っていません。
実行した検証と確認範囲は [検証記録](docs/validation.md) にまとめています。

## Scarlet への組み込み

[`platforms/scarlet/bundle.toml`](platforms/scarlet/bundle.toml) をイメージの bundle に追加すると、
`/bin/cadence` と stemd のアプリ登録を配置できます。script layer が PIC の標準ライブラリーと
本体をビルドし、実際に配置するファイルの ELF 検査に合格してから配置します。
イメージビルドは Scarlet Rust、Python、`readelf` / `llvm-readelf` がある開発 shell 内で行います。
スクリプト内では新たな Nix shell を起動しません。既存の `file-music` アイコンを使い、
Files から音声ファイルを開くための MIME type も登録します。
Scarlet 本体の既存 manifest はこのリポジトリから変更しません。

実行には SWS、SAS、`/bin/scarlet-ld`、対応する SGFX ドライバーと manifest を含む
Scarlet デスクトップイメージが必要です。通常の native SGFX bundle を先に組み込んでください。
ファイル選択サービスがない場合も **Browse folder** を利用できます。

ネイティブ executable は `build.rs` のリンク専用 seed を介して SGFX の
`dlopen` / `dlsym` / `dlclose` / `dlerror` を resident loader へ解決します。seed 自体は配布しません。
`verify-scarlet` はその 4 symbol 以外の未解決参照、不要な共有ライブラリー依存、
未対応の relocation を拒否します。bundle 以外の方法で配置する場合も、
**配布前には必ず実際に配置するバイナリーを検査してください。** 別設定でビルドした場合は次を実行します。

```sh
./scripts/dev python3 scripts/audit-scarlet.py /path/to/staging/bin/cadence --arch aarch64
# RISC-V64 は --arch riscv64
```

その後 Scarlet 上でファイル追加、再生 / 一時停止、シーク、曲送り、EOF、音量、
アプリ終了を操作して、画面と出音を確認します。起動・再生エラーは画面下部に表示します。

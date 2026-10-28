# 検証記録

## 2026-10-28 — Kanade のライブラリー同期とクライアント証明書

Cadence `877f521` をベースに、Kanade develop
`e00a3bf531e4598e717dbdfd1ba6cea9c641308c` の Rust protocol / server / DB 実装と
KanadeApp の既存 Keychain 実装を参照しました。

- `./scripts/dev cargo test --locked --workspace -- --test-threads=1`:
  **263 passed / 0 failed / 1 ignored**。ignored は既存の実 CLAP Gain を要求するテストです。
  app 103、sources 62、その他 98 が成功しました。
- 最後にカタログの累計メタデータを 64 MiB 以内に制限し、sources 62 件を再実行して成功。
  フォーマット検査と `scripts/bundle-macos --release` も成功し、Apple Silicon 用の
  `target/macos/Cadence.app` を作成しました。
- 全曲 API `get_tracks` を 500 曲ずつ取得し、アルバム未分類の曲を含めて同期。
  重複ページ・不正応答・上限超過・キャンセル時には途中結果を採用しません。
  source 単位の置換は保存に成功してから UI に反映し、他のソースの曲を保持します。
  削除と同期の競合、保存失敗、空の結果、設定の read-only、起動時の同期を検証しました。
- 署名 URL を新しい Range 要求ごとに取得する fixture で、順次再生・逆方向シーク・
  更新失敗・キャンセルと heartbeat を検証。認証付きメディアの別 origin への
  リダイレクトを、接続前に拒否することも確認しました。
- 合成の CA / client certificate / PKCS#12 による、クライアント証明書必須の HTTPS と
  WSS の往復に成功。実 Keychain へのアクセス・承認ダイアログは実行していません。
  実証明書は明示的な **Authorize certificate…** 操作だけで読み取る設計で、
  background は当該アプリ起動中に承認済みのメモリー内キャッシュだけを使用します。
- `kanade_smoke` を loopback fixture に実行し、2 曲の Library 登録・検索、64×64 カバーの
  デコード、44.1 kHz PCM のデコード、5 秒位置へのシーク、別セッションでの再開に成功。
  音源とカタログを保存せず、音声出力やサーバーの再生操作を行わない検査です。
- 実際の `https://kanade.ichigo.dev` はサーバー証明書の検証に成功し、クライアント証明書の
  TLS CertificateRequest と nginx の HTTP 403 を返しました。Mac の DNS / route / curl の
  socket address で LAN 内の直接接続とプロキシ不使用を確認しました。
  Cadence の新しい API でも HTTP 403 を明示した接続エラーを確認しました。
  実証明書は利用せず、実ライブラリー・画像・再生の検証は接続条件の確認または所有者の
  明示的な証明書選択・システム承認待ちです。サーバー・ネットワーク設定は変更していません。
- CUA の `cua.getApp` で独立した `Cadence Kanade QA.app` を開く操作は、承認待ち後に
  `aborted by user` となりました。UI 状態が返らず起動結果は不確定のため、再試行・入力操作を
  行わず、ネイティブ画面での確認は未完了です。テストサーバーは停止済みです。
  独立した CPU renderer の検査は完了し、最小／標準サイズの 44 画面を生成。
  Kanade 接続フォームと証明書承認画面の画像を目視し、重なり・切れがないことを確認しました。
  これはネイティブウィンドウ操作や OS の認証ダイアログの検証とは別です。

ログと fixture は `artifacts/kanade-integration/` にあります。
Kanade の DB エラーが空配列になる仕様と、ページングに snapshot/revision がない制約は
サーバー側に残ります。Scarlet / Linux の実行と、実サービスからの出音は未検証です。

## 2026-10-06 — キューのハイライトとギャップレス再生

- Play queueの色はcurrentのみを参照し、クリック時の選択を併せて着色する条件を撤去。
  currentの変更時には古いselectedを解除。保持した描画ツリーで0→1→2→0と進め、
  常に正しい1行だけがハイライトされることを確認しました。
- 自然な曲送り／Repeat One／Repeat Allは、次曲を別の単一ワーカーで先読みして、
  同じ音声出力に連続してPCMを送ります。CLAP stateを曲境界でresetせず、
  current／duration／seekable／positionは出力の未消費フレームと境界位置から更新。
- 8,000／16,000／11,025 Hzの3曲のPCMが、各曲を出力レートへ変換した期待値と
  全フレーム完全一致。曲間の無音挿入・欠落・重複なし、出力生成1回、drainは末尾の1回。
  曲境界のPCMがバッファーにある状態で一時停止／再開しても消費PCMが一致します。
- Repeat Allの折り返し、Repeat One、シークとキュー編集による先読み破棄、
  詰まった先読みのキャンセル、次曲の失敗時に現在曲のtailが残ることを検証。
  曲境界とコマンド受付が重なる場合は先読みPCMを保持して遷移を延期します。
  ネットワークの待機を出力停止と誤判定せず、terminal drain開始後の追加は旧tail完了後に
  出力を開き直します（Scarletのdrain済みストリームに追記しません）。
- Macのrelease Gapless QA版で6／18／18秒の連続440 Hz WAVをキューに追加し、
  最初の行をクリックしてA→B→Cの自動曲送りを実画面確認。AとBの色が残らず、
  Now Playingの曲名も再生中の行と一致。音声ループバック計測ではなく、PCM検証と
  ネイティブ画面の動作確認です。専用データは `/tmp/cadence-gapless-qa-20261006`。
- release workspaceは実CLAP Gain／ignoredを含む282件成功、format check成功。
  通常bundleと既存Resize QA bundleを更新。Scarlet AArch64／RISC-V64のreleaseビルドと
  ELF監査も成功。Scarlet実機のSASでの聴感検証は別途です。
- encoder delay／padding除去は[Symphonia 0.6.1の形式・コーデック対応](https://docs.rs/symphonia/0.6.1/symphonia/)に依存。
  AAC／MP4、元ファイル内の無音、ネットワーク供給不足までサンプル単位の境界を保証しません。
- ログ: `artifacts/kanade-ui-qa/cadence-gapless-{workspace-tests,format-check,bundle,scarlet}.log`、
  `gapless-live*.log`、`gapless-updated-qa*.log`。

## 2026-10-06 — トランスポートの中央配置を固定

再生ボタンはフローティングバー全体の中央、前後移動は左右対称。
左右同幅の情報／音量列と、Repeatに対応する左側の余白で配置を維持します。
既存の320〜900pxの単体検証に加え、アプリ全体を同じElementTreeで保持したまま
737〜1600pxで縮小／拡大、キュー開閉、再生／一時停止、Repeat Off／One／All、
シーク可否、長短の曲名を変更する54条件を追加。各条件でページ中央からのずれと
前後移動の対称性を0.5px以内で検証しました。release QAアプリの実画面でも
ウィンドウ縮小後と広い幅でバー中央に再生ボタンがあることを確認。
release workspaceの最終確認は273件成功（実CLAP Gain／ignoredを含む）、
`cargo fmt --all -- --check` も成功。
ログ: `artifacts/kanade-ui-qa/cadence-transport-center-tests.log`、
`artifacts/kanade-ui-qa/cadence-precommit-{tests,format}.log`。

## 2026-10-06 — Now Playingの文字幅による省略

曲情報の文字数による省略と最低8文字の条件を撤去。
タイトル11px、アーティスト／アルバム10pxの実フォント描画幅で、省略記号も含めて
列内に収まる最長の文字列を選択します。日本語が再生コントロールやシーク時刻の
領域へはみ出すのを防ぎます。84〜240pxの情報列と再縮小で、全3行の描画範囲が
列内にあることを検証。release workspace 272件成功、通常／QA bundleを更新。
ログ: `artifacts/kanade-ui-qa/cadence-track-label-{tests,workspace-tests,bundle}.log`。

## 2026-10-06 — ネイティブ装飾・リサイズとリスト／Now Playingの配置

- ScarletUIは `86a765daaf26545fc0294aa7054d98630df9e368`、Resonaraは
  `7b17e9f87714d47f572526dbd239282ab4745ce3` に固定。通常のGit依存でrelease bundleを再ビルド。
- winitはシステム装飾、SWSはScarletUI装飾をデフォルトとし、明示指定は維持。
  既存のgeometry callbackでコンテンツサイズを取得し、32pxの手動補正を撤去。
- macOSではネイティブイベントループ内でレイアウトとGPU描画を更新。
  live resize中にpumpでイベントループを停止する構成を撤去し、通常クリックを壊した
  mouse-button pollingも撤去。サイズ通知はOSへの明示リサイズ要求と分離。
- キュー、アルバム詳細、Sources、Effect chainの2行ラベルは、固定高さを外した
  自然な高さのVStackをHStack内で中央配置。間隔は3px。番号・時刻・アイコンも中央に配置。
  スキャンのエラー行、fallback browserの種別、Effectパラメータの値も整列。
- コンパクトNow Playingも左右の列を同幅にし、再生ボタンをバー全体の中央へ配置。
  Sliderの最低100px強制を修正し、狭い領域へ収める。320〜900pxと639/640pxの
  切替境界で再生位置とコントロール重なりをチェックする回帰テストを追加。
- Macの独立QA bundleでアルバム詳細の2行、Effect chainの2行・番号・アイコン、
  最小幅のNow Playingを実画面確認。通常bundleとQA bundleの実行ファイルは同一。
  UI操作ツールの再接続後にQAのEffects遷移を確認。検証用データは `/tmp/cadence-resize-qa-20261006`。
- release: Cadence 271件成功（実CLAP Gainを含む）、ScarletUI 473件成功、
  Resonara 407件成功・既存opt-in 21件ignored。描画検証を含むsuiteは共有のUI環境を
  他テストが変更しないよう `--test-threads=1` で実行。
- Scarlet AArch64／RISC-V64 releaseビルドとELF監査成功。Scarlet実機の動作検証は別途。
- ログ: `artifacts/kanade-ui-qa/{cadence-final-tests,cadence-final-bundle,cadence-final-scarlet,scarlet-ui-final-tests,resonara-slider-git-tests}.log`。
  実ScarletUI rendererによる全ページの描画は `artifacts/kanade-ui-qa/cells-final/`。

## 2026-10-06 — ソース削除とCLAP標準検索

- ソース削除で対応する曲も永続カタログから除去し、Albums / Artists / All songsへ反映。
  ローカルの音声ファイルは変更せず、別の登録フォルダーが共有する曲は保持します。
  オフラインのフォルダーと、旧処理ですでに登録解除されたフォルダーも除去できます。
  外部ソースの接続設定はカタログ保存成功後に解除します。
- ローカル／外部の実ポインター操作テストで、一覧から消えること、保存後の再起動、
  他のソースと音声ファイルが残ることを確認。保存失敗では元のカタログと接続設定を保持。
- 旧処理で残っていたfixture 5,381曲を、元のカタログをバックアップしてから除去。
  実ライブラリーの5,381曲と `/Volumes/music/Music` の登録は維持しました。
  バックアップは `~/.local/share/cadence/library-before-fixture-removal-20261006-020747.json`。
- 起動失敗の `.ips` は保存済みFabFilter Pro-Q 4がある状態でAppKitの
  `NSAssertMainEventQueueIsCurrentEventQueue` によりSIGTRAP。
  UIの最初のOSイベント処理後にチェーンを復元するよう変更しました。
  元のプラグイン設定／opaque stateを保持したまま、QAのrelease版と通常bundleを起動。
  QAのプロセスsampleにはPro-Q 4のロードとメインスレッドのAppKitイベント処理が記録されています。
- CLAP検索はResonaraのcatalogと[CLAP entry.h](https://github.com/free-audio/clap/blob/main/include/clap/entry.h)に沿い、
  OSごとの標準ディレクトリ、`CLAP_PATH`、実行ファイル隣の `plugins/` を再帰検索。
  Effectsを開いた際に自動実行し、更新ボタンで再検索できます。重複／循環／bundle内部を除外し、
  16,384エントリ／1,024ライブラリー／深さ32に制限します。
- 検索とdescriptor取得は独立ワーカー。実Resonara Gain fixtureで、検索待ちでも
  音声処理が継続し、descriptorが公開されることを確認。
  復元失敗後の検索成功では空のフォールバックチェーンを保存しない回帰テストも追加。
- ScarletUIは `f0a8b770352bc0b9e57df5c88a7520723a147114`、Resonaraは
  `d711731e016a5bf4e84cc2442f3b1648bce80946` の通常Git固定参照。
  UI全8crateの参照を統一。今回のUI共通余白APIはcore 413件＋doc 23件、
  Resonaraの参照更新はrelease 407件成功／21件ignoredを確認して公開済み。

Cadence workspaceのreleaseテストは実CLAP fixture／ignoredを含めて270件成功。
最新のmacOS release bundleと、Scarlet AArch64／RISC-V64のreleaseビルド・ELF監査にも成功しました。
最終の通常bundleでもPro-Q 4のロードとAppKitイベント処理をprocess sampleで確認し、
標準エラー出力は空でした。Scarlet上のSWS/SAS実行・出音とLinux/Windows実行は未検証です。

今回のログは `artifacts/kanade-ui-qa/source-effects-*`。
Mac操作ツールは全アプリのウィンドウを取得できない `cgWindowNotFound` 状態のため、
今回の最終版で標準検索一覧を実画面から操作する検証は未完了です。


## 2026-10-05 — GPU描画の保持と内容更新

- ScarletUIを`fae2b408a957549a585a867cc37aefda397d5820`、Resonaraを
  `5410b70f148382d5c9cd8a723b80f1a7e3343100`へ固定。ローカルUI patchを使わず、
  Cargo.lock内の7個のUI crateが同じGit参照であることを確認しました。
- GPU向けのローカル描画記録と頂点バッファを保持し、スクロールでは位置・clipを更新。
  仮想化で増えた行、内容変更、画面外の更新、入れ子のスクロールは適切に記録を更新します。
  画像の共有は行をまとめた後も維持。CPU描画経路は既存の方式を使用します。
- アルバムGridと詳細Listに安定したモデルIDを設定し、上方の追加・削除に対する
  スクロール位置の保持へ対応。明示的な選択移動は優先し、内容縮小時は範囲内へ収めます。
- MacのSGFX/WGPUには描画uniform bufferの再利用も反映。
  `a1156b12083017abfd7e64a7caf37a152ace6d74`に固定しています。
- ScarletUIはcore 410件、doc-test 23件、SGFX 74件、Winit 26件に成功。
  Apple M3 Pro/MetalでSGFX WGPU 42件にも成功。UI本体のScarlet AArch64/RISC-V64と
  RISC-V64 legacy stdのrelease checkも成功しています。
- 自動スクロールは50,000行、文字・画像・複雑な行、内容更新あり/なし、保持あり/なしで比較。
  主比較は各8秒を2回実行。頂点データの再送は約97.8%減りました。
  ただし表示待ちが支配的で、文字だけの一覧のp95には明確な改善がありません。
  Macの結果はScarlet上のFPSや物理scanoutの計測ではありません。
- 通常のGit参照によるCadence workspaceのreleaseテスト254件に成功
  （CLAP実fixture・ignoredを含めて直列実行）。Resonaraは407件成功、21件は既定のignored。
- 最終releaseのGUI検証は別のデータ保存先を使い、再生中の元ウィンドウを維持。
  コピーした10,762曲/716アルバムで、画像付き一覧、383曲の詳細、全曲一覧を
  CUAの実wheel入力で往復スクロール。行・アートワークの欠落は見られませんでした。
  部分更新が角丸clipと交わらない場合に出た起動時エラーを修正し、回帰テストを追加しました。

自動計測の手順・各runのp95/p99/max・制限事項はScarletUIの
`docs/SCROLL_PERFORMANCE.md`と`docs/scroll-retained-20261005.json`。
画像共有の最後の修正後も画像付き一覧を8秒ずつ（内容更新あり/なし、保持あり/なし）比較。
その4 runは`artifacts/kanade-ui-qa/shared-artwork-final-results.json`に記録しています。
正式なGit固定参照から作った最終releaseでもアルバム000→019→000の往復を再確認し、
描画エラーなし。追加記録は`artifacts/kanade-ui-qa/retained-git-final-gui-summary.json`。
GUIの全セッション記録は`artifacts/kanade-ui-qa/retained-final-gui-summary.json`です。
画像の初回読み込み・navigationを含む1,087フレームでは、中央値3.406ms、p95 25.312ms、
最大130.281msでした。gesture区間だけのFPSや無条件に120fpsという主張ではありません。

## 2026-10-05 — ScarletUIの共有参照へ統合

- ScarletUIのスクロール修正を`a9ec0b8b622456a698e6ffc7fc36cf82f862ce7a`として公開し、
  Cadenceと他のアプリの依存参照・Cargo.lockを更新。17個のlockfileでUIのGit参照が1種類であることを照合。
- CadenceのScarletUI core・SGFX renderer・Winitのvendorコピーとpatch指定を削除。
  Resonaraも`a5defc520baa3a84cf9eb7ffe2f74ae2197891e5`へ更新し、CLAP経由の旧UI参照を解消。
- 統合後のCadence workspaceはreleaseで254件成功（実CLAP fixture・ignoredを含めて直列実行）。
  うち47件は同梱SWS protocol。UI本体のテストは上流へ移ったため、以前の771件とは集計範囲が異なります。
- Resonara workspaceはreleaseで407件成功、21件は既定でignored。
  CadenceのmacOS releaseバンドルを再生成しました。

参照の照合とビルド・テストログは`artifacts/dependency-refresh/`。
`reference-audit.json`に固定SHA、17個のlockfile、テスト集計を記録しています。

## 2026-10-05 — ジェスチャーの引っかかりと実操作検証

- ScarletUI本体にも描画・画像・仮想化の修正を反映。既存の行はスクロールごとに再レイアウトせず、
  新しく入る行だけを測定します。リサイズ・view更新時は保持行も再測定します。
- SGFXが位置付き文字列を共有キャッシュから取得するよう変更。日本語を含む画面で、個別glyphの
  キャッシュが溢れても同じ文字列を再ラスタライズしません。512件・4MiBに制限、warm取得の確保0を検証。
- Winitの小さなHiDPI移動量をイベントごとに丸めて失っていた問題を修正。端数を次のサンプルへ保持。
  両方向、反転、開始・終了・キャンセル、scale変更、ホイールとの切り替えを検証。
- 一度動き出した単一軸のトラックパッド操作は、途中の斜め成分で方向を判定し直して入力を捨てません。
  新しいジェスチャー・終了・キャンセルで解除。離散ホイールと入れ子の初期方向判定は維持します。
- 同じイベントbatch内の連続Movedを合算する処理を既定で有効化。追加の16ms待ちは入れず、
  phaseや途中の別イベントの順番を保持します。`SCARLET_UI_APP_WHEEL_COALESCE=0`で比較できます。
- Macのreleaseを実際に操作し、全曲一覧の小刻みスクロールとMOTHER3の285曲詳細の往復を確認。
  この時点では音楽volumeが未接続のため、実ライブラリの画像込みの再検証はできていませんでした。
- ユーザーの指示で通常の設定directoryを
  `~/.local/share/cadence.before-scroll-fixture-20261005-170536`へ退避して初期化。
  `scripts/create-scroll-fixture.py`で5,381曲・358枚のPNG・383曲の2discアルバムを生成し、
  通常のdata directoryにテストカタログを配置。音源は再生可能な2秒の小音量テスト波形です。
  画像と音源は`artifacts/kanade-ui-qa/scroll-fixture-music`にあります。
- 最終releaseの実操作sessionでは4,335提示frameを記録。描画/backendの中央値7.863ms・p95 8.201ms、
  入力dispatch中央値34µs・p95 172µs。起動・画面遷移・リサイズも含むsession最大値は130.804msです。
  GPUのscanout FPSを測った値ではありません。ユーザーによる最終版の操作で「ヌルヌルになった」と確認。
- Workspace771件（ignored・実CLAP fixture・UI直列）のテスト、macOS release bundle、
  Scarlet AArch64 / RISC-V64 releaseとELF auditに成功。Scarlet実機のスクロールFPSは未測定。

主な記録: `native-fixture-scroll-final.log` / `native-fixture-scroll-final-error.log`、
`native-modern-songs-gesture.log`、`native-modern-detail-gesture.log`、
`native-modern-detail-scrolled.png`、`scroll-modern-complete-tests.log`、
`scroll-modern-release.log`、`scroll-modern-scarlet-final.log`（すべて`artifacts/kanade-ui-qa/`）。

## 2026-10-05 — スクロール性能・GPU描画・タグと全アルバムの表示

- GPU提示の前にUIをCPUで画像化するキャッシュ経路を除去。SGFX/wgpuには直接の描画指示を渡し、
  CPUフォールバックだけがラスタキャッシュを使用します。Mac releaseの実プロファイルで
  `flush_picture_chunk` / `compact_picture_chunk` / `render_raw_image`が出ないことを確認しました。
  提示経路にはMetalの`nextDrawable`待ちが残ります。FPSや体感の改善率は未計測です。
- 標準Imageは元画像のBufferを共有し、GPUへ表示・切り抜き矩形を渡します。スクロール・リサイズで
  カバーをCPUで拡大縮小し直しません。画像用の領域確保0、Buffer identity/revision保持、
  warmフレームの再アップロード0を検証。window 2x / source 1xでも表示サイズを正しく扱います。
- 仮想一覧の新しい行を生成した際に描画が更新されない問題を修正。GridViewの末尾アルバムと
  ListViewの末尾曲をピクセルで確認。RepaintBoundaryを含む場合と直接GPU指示経路も対象です。
- CPU用の疎な描画チャンクも全リストの高さの画像を確保せず、描く範囲だけを確保。
  400×100,000・2xのコンテンツ内の1行を描いても、確保量1MB未満を検証しています。
- 詳細行はタグのdisc / track番号を表示し、disc→track→titleで並びます。番号無しは「—」です。
  旧カタログはワーカーでタグを補完し、成功状態を保存。起動後の実カタログは5,381曲・
  trackタグ5,085曲・補完完了の状態を確認しました。これはファイルに番号が無い曲も含みます。
- 5,381曲・358アルバムのコピーで最適化版を起動し、元画像をGPUで拡大縮小したギャラリーを
  実画面で確認。画面記録は`artifacts/kanade-ui-qa/native-gpu-gallery.png`です。
  実機での全末尾項目の手動照合は未完了で、到達性・描画は上記の自動テストで検証しています。
- 同数の合成カタログでギャラリーと曲一覧を240フレームスクロールし、描画指示が可視行数に
  制限されることを検証。releaseの指示準備時間は中央値353µs・95%点456µs・最大782µs。
  画像I/Oと実GPU実行・提示時間を含まない測定です。実機全体のフレーム時間ではありません。
- Cadence207件、ScarletUI core397件、SGFX renderer65件、SWS47件、Core doc-test23件に成功。
  UIテストは直列、実CLAP fixtureを指定。Cadence各crateのall-targets Clippyにも成功。
  macOS releaseバンドル、Scarlet AArch64 / RISC-V64 releaseとELF検査に成功しました。

主要ログは`scroll-complete-tests.log`、`scroll-final-clippy.log`、`scroll-scaling-release.log`、
`scroll-final-scarlet.log`、`scroll-release-measurement.log`、`native-gpu-scroll.sample.txt`です。

## 2026-10-05 — アルバム・一覧・部分描画の修正

- ローカルアルバムをKanadeと同じ親フォルダー単位に変更。実カタログのコピー
  5,381曲では797グループから358フォルダーへまとまります。既存データは再スキャン不要で
  グループ化されます。追加タグ取得は上記の起動時補完へ変更済みです。
- Play allの回帰テストは実デコーダーと無音WAV6曲を使い、1280×800・1512×1429の2倍描画で
  画面保持、キュー6曲、Playingへの遷移、変更のないラスターバッファの再利用を確認。
- GPUと同じ物理更新範囲で消去するバックエンドで、キューと再生バーの同時更新が
  未変更の内容を消すことを固定した上流で再現。修正後は保持されます。
- 部分更新範囲を統合した後の矩形同士の重複も再現・修正し、半透明の内容を二重に合成しません。
- キュー、検索、アルバム詳細、登録フォルダー、ソースの各一覧を100件から6件へ縮小し、
  実ListViewの末尾位置と範囲が一致することを確認。Coreでは3件から150件への拡大も確認。
- 1,000件の一覧をスクロールした際のClone回数を計測。行ごとの一覧全体コピーを除き、
  スクロールで生成する行だけをコピーする回帰テストに成功。
- 画像ごとのrepaint boundary、ワーカー内でのBitmapImage変換、未表示画像の待機キャンセルを確認。
  画像履歴256サイズを一つずつ描画した際のテクスチャ枯渇も再現・修正。128枚の同時使用は成功し、
  129枚の同時使用は従来どおり容量エラーです。128領域がすべて小さい既存割当で埋まった場合、
  それ以上のサイズにはなお容量エラーがあり得ます。
- Cadence 203件、SWS 47件、ScarletUI core 391件、SGFX renderer 64件のテストと
  Coreの23件のdoc-testに成功。UI描画テストは共有される描画スケールの競合を避けるため直列で実行。
  Cadence各crateのall-targets Clippyも成功（上流由来のvendor警告は対象外）。
- macOSバンドルはreleaseが既定。最適化ビルド、plist検査、Scarlet AArch64 / RISC-V64の
  releaseビルドとELF検査に成功。

この段階でのmacOS実画面・スクロール負荷の確認はMacのロックにより保留でした（上記に追加検証）。
最適化版の検証用インスタンスは独立したデータ保存先で起動済みですが、
今回の変更について体感速度の改善率や実音声の確認はまだ主張していません。
ログは`artifacts/kanade-ui-qa/`に保存しています。

## 2026-10-05 — macOS ネイティブ操作

Mac のロック解除後、Apple Silicon macOS で最適化ビルドの `Cadence.app` を起動し、
実際のウィンドウとネイティブファイルダイアログを操作しました。
設定とライブラリーの保存先は `artifacts/mac-host-qa/state` に分離しています。
素材は生成した低音量の 45 秒／6 秒 WAV と、同じフォルダーの `cover.png` です。

- **Add music** のネイティブダイアログで WAV を開き、キューへの追加とカバー表示を確認。
- 再生時間の進行、一時停止中の時間保持、左矢印キーによる 5 秒シークを確認。
  実際の出音を耳で確認したという意味ではありません。
- 実 Resonara Gain を **Discover** → **Add selected** で読み込み、汎用 UI で
  Gain を `1.000` から `0.494` に変更。バイパスの切り替えと `app.json` への保存を確認。
- **Library** へ素材フォルダーを取り込み、2 曲・エラー 0 件で完了することを確認。
- 再生情報に変更がない場合にも表示を更新していた処理を修正し、回帰テスト 2 件を追加。
  app の 38 テストと Clippy に成功しました。
- 赤いテーマは offscreen renderer の 18 画面と、再ビルドしたネイティブウィンドウで確認しました。
  ボタン、選択状態、アイコン、プレースホルダーをスライダーと同系色に統一しています。
- ウィンドウを閉じて終了し、再起動後にライブラリー 2 曲と Resonara Gain の
  有効状態・パラメーター `0.494` が復元されることを実画面で確認しました。

ローカル用 macOS バンドルを作る `scripts/bundle-macos` も追加し、実行ファイルのコピーと
plist の検査に成功しました。現在はreleaseが既定で、`--debug`を明示した場合だけ未最適化です。

```sh
./scripts/dev scripts/bundle-macos --release
open target/macos/Cadence.app
```

既知の制約として、使用中の ScarletUI のテキスト欄では **Cmd+A** で `a` が入力される挙動が
残っています。検証ログと素材は `artifacts/mac-host-qa/`、バンドル検証ログは
`artifacts/mac-host-qa-bundle.log` に保存しています。
Scarlet／Linux／Windows の実行と、外部サービスの実サーバーとの相互運用は今回も未検証です。

## 2026-10-04 — ビルド・自動検証

Apple Silicon macOS、リポジトリの固定 Nix 開発環境で確認しました。

- `./scripts/dev bash scripts/verify`: フォーマット、workspace テスト、ホストビルド成功。
  実 Gain を指定し、app 36、core 27、effects 3、library 18、platform 12、sources 25 の 121 件が成功。
  effects の通常は ignored の実 Gain テストも別途実行し、Cadence は計 122 件が成功しています。
  vendor の SWS protocol の 47 件も成功しています。
- Cadence の 6 crate に対する `cargo clippy --locked --all-targets -- -D warnings`: 成功。
  vendor は upstream の lint があるため、このチェックの対象には含めていません。
- AArch64 / RISC-V64 の Scarlet release ビルドと ELF 検査に成功。
  標準ライブラリーも PIC で再ビルドし、未解決参照は SGFX / CLAP が使用する loader API 4 個のみ。
  実行時の共有ライブラリー依存や TLS はありません。
- 1 秒の無音 WAV を `audio_smoke` example で実際の CPAL 出力へ送信し、再生位置が進み、
  自然な EOF で drain が完了することを確認しました。これは音を耳で確認するテストではありません。
- GUI プロセスの起動と SGFX / wgpu backend 選択を確認しました。
  Mac のロックによりネイティブウィンドウの操作確認はできていません。
- ScarletUI の ElementTree を使うクリック配送と、CPU renderer による標準・最小サイズの
  キュー／画像／ブラウザー／Library／Sources／Effects の計 20 画面を検証しました。
  CLAP を読み込んだ状態を含み、描画結果も確認しています。
  これは OS のウィンドウ操作、GPU 表示、ネイティブファイルダイアログの実動作確認とは別です。
- CLAP は実際の Resonara Gain dylib で、探索、チェーン処理、状態保存・復元、並べ替え、バイパス、
  削除、サンプルレート変更、100 回のロード・破棄を検証。quarantine 数の増加はありません。
  UI でも実ポインターイベント配送で Discover → Add → Gain 変更 → bypass → remove を確認しました。
- 同じ Gain の Scarlet AArch64 / RISC-V64 ELF もビルド・監査成功。
  未解決 import、依存 DSO、TLS はなく、動的 relocation は各 CPU の RELATIVE のみです。
  これは Scarlet 上でのプラグイン実ロード・音声処理の確認ではありません。
- Kanade は loopback の WebSocket＋HTTP fixture で、カタログ、署名 URL、取得中のセッション保持、
  heartbeat、音源と画像を検証。DLNA は device description、SOAP paging、DIDL、キャンセルを検証。
  接続先の実サーバーや実機での相互運用試験は未実施です。
- 外部ソースは実 subprocess と loopback HTTP で browse / resolve / artwork を検証。
  応答サイズ上限、壊れた応答、応答拒否、タイムアウト、キャンセルも対象です。
- カタログと設定の atomic save、破損・将来バージョンの保護、終了時 flush、タグ・埋め込み画像、
  画像展開上限、非同期要求の連打・キャンセル・古い結果の破棄、追加と再生の原子性を検証しました。

テスト素材は生成した短い音源で、MP3 / FLAC / Vorbis / AAC / AAC-M4A / ALAC / AIFF / CAF の
デコード、シーク、EOF 後の再開を確認しています。長い Vorbis のシーク位置、途中で切れた FLAC、
部分書き込み、出力待ち、キュー操作、エラー復帰も対象です。

Scarlet 上での GUI・SAS 再生・CLAP 実動作・出音、Linux 実行、Windows 実行は未検証です。
Scarlet の HTTP/WS、host の HTTPS/WSS をコンパイルしましたが、実サービスへの TLS 接続は未検証です。
Scarlet の外部プロセス型ソースは、標準ライブラリーの `Child::kill` が未対応のため明示的に無効です。

## 再現

```sh
./scripts/dev bash scripts/verify
./scripts/dev bash scripts/verify-scarlet
# 任意の出力場所へホスト用 Gain をビルド
./scripts/dev cargo build --release --manifest-path ../resonara/plugins/resonara-gain/Cargo.toml \
  --target-dir /tmp/cadence-native-gain-fixture
# macOS の例。Linux は libresonara_gain.so、Windows は resonara_gain.dll
CADENCE_TEST_CLAP=/tmp/cadence-native-gain-fixture/release/libresonara_gain.dylib \
  ./scripts/dev cargo test --locked -p cadence-effects --test native_gain -- --include-ignored
CADENCE_TEST_CLAP=/tmp/cadence-native-gain-fixture/release/libresonara_gain.dylib \
  CADENCE_RENDER_UI="$PWD/artifacts/ui" ./scripts/dev cargo test --locked -p cadence --bin cadence
./scripts/dev cargo clippy --locked -p cadence -p cadence-core -p cadence-platform \
  -p cadence-library -p cadence-sources -p cadence-effects --all-targets -- -D warnings
# 10 秒未満の音声で、実際の既定出力デバイスを確認
./scripts/dev cargo run --locked -p cadence --example audio_smoke -- /path/to/short.wav
```

ログ・ELF 検査 JSON は `artifacts/`、CPU 描画は指定したディレクトリーの PPM に出力します。
ネイティブ Gain の検査結果は `artifacts/clap-gain-{aarch64,riscv64}/build.json` です。
描画用テストの曲名はテストデータで、通常起動のキューは空です。

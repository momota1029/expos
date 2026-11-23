---
goal: expos のバージョン別インストール、更新、チャンネル（beta と stable）の仕組みを作る
done_condition: Windows で expos を `%LOCALAPPDATA%/expos/v<version>/` に配置し、共通ランチャーから選択中の版を起動できる。`main` の更新ごとに beta を、手動タグから stable を配布できる。アプリ内で更新とチャンネル切り替えができ、データはチャンネルごとに分離される。`cargo xwin build` の成果物で開発版をその場で更新できる。一時インストール先を使って Windows 実機で一連の動作を確認する。
status: done
pace: normal
verification: Windows デスクトップで、更新メニューと通知からの操作、チャンネル切り替え後のデータ分離、更新後の再起動とセッション復元を確認する。GitHub 側では、push とタグ作成の承認後に GitHub Actions での beta・stable の配布と、GitHub Releases からの実ダウンロードを確認
stale: false
---

## 済んだこと

```yaml
- '`~/rust/expos` の `main` を調査し、配布先が `https://github.com/momota1029/expos.git`、既存公開版がプレリリースの `v0.1.0-beta.1` であることを確認した。`.github/workflows/release.yml` は既存タグを指定する手動実行のみで、Windows 上でタグと Cargo の版を照合してドラフトを作る。'
- '`src/main.rs` などに未コミットの変更があることを確認した。今回の調査では、ビルドとテストは未実施。'
- 作業報告では、Windows でランチャー経由の stable と beta がそれぞれ `%LOCALAPPDATA%\expos\data\stable\` と `%LOCALAPPDATA%\expos\data\beta\` を使うよう実装済み。絶対パスの `EXPOS_DATA_DIR` を優先し、直接起動時は `%LOCALAPPDATA%\gpui-filer\v6\` を使う。
- 作業報告では、各チャンネルの初回起動時に、セッション復元に先立って従来データをコピーする処理を `src/app_paths.rs` と `src/main.rs` に実装済み。変更は未コミットで、報告が途中で切れているためテストの結果は未確認。
- 作業報告によると、`release/Install-Expos.ps1` と `release/Uninstall-Expos.ps1` を実装した。両スクリプトは `release/Install-Common.ps1` を使い、Rust ランチャーと同じ `.lock` の先頭 1 バイトをロックする。インストーラーは準備済みの版を配置してから `current` を原子的に置き換える。
- アンインストールの既定動作で `<root>\data` を保持し、再インストール時に再利用できるようにした。`-RemoveData` は確認付きでデータを削除し、`-Force` で確認を省略できる。実行中のファイルがある場合は削除を拒否する。
- '`release/README.md` にインストール構成と試験手順を記載し、`release/Test-Install.ps1` に更新、ショートカット、不正なチェックサムと ZIP エントリ、ロールバック、実行中の版の保護、データの保持と削除の試験を追加した。'
- 作業報告によると、ランチャー経由でインストールした Windows 版向けのアプリ内アップデーターを `src/update/` に実装した。GitHub Releases とローカルディレクトリを更新元として扱い、バックグラウンドで更新を適用する。再起動時はウィンドウセッションを保存してから共通ランチャーを起動する。
- 作業報告によると、通常の更新では実行中の版より新しい版を選び、beta の更新候補には stable も含める。明示的なチャンネル切り替えで旧版に移る場合は追加確認を表示し、対象の版が実行中なら設定だけを更新する。
- 作業報告によると、WSL2 向けの `scripts/dev-install.sh` を実装し、`release/README.md` に使い方を記載した。`cargo xwin build` のデバッグ成果物と実行時シェーダーを固有の `v<core>-alpha.dev.<UTC timestamp>.<UUID>` に配置し、共通インストーラーで `current` を切り替える。既定では開発版を 5 世代保持し、選択中の版、実行中の版、配布版を整理対象から外す。
- 開発版インストールについて、作業報告では `bash -n`、WSL のモック試験、Rust の 12 件のチェック、Windows ターゲットの型チェック、PowerShell の静的構文チェックが通過した。Windows 実機での連続インストールとシェーダーの読み込みは未確認。
- '`scripts/dev-install.sh` が Windows パスを `printf` の引数として渡すように修正した。作業報告では、スクリプト全体を点検し、ドライラン出力に `gpui-filer\v6` が文字どおり含まれ、垂直タブが入らないことを確認した。修正後の `bash -n`、`--help`、LF 改行の確認も通過した。'
- '`release/Test-Install.ps1` の試験用ディレクトリを `%LOCALAPPDATA%\Temp` 配下に変更した。`LOCALAPPDATA` が未設定の場合は `%TEMP%` を使う。'
- '`release/Install-Common.ps1` は、両方のボリューム GUID パスを取得できた場合はその結果を比較し、いずれかの取得が Win32 エラーになった場合は正規化した絶対パスを大文字小文字を区別せず比較する。後者では表記が異なる同一パスを識別できない。報告された `Y:` エラーの原因と、修正後の Windows 実機での動作は未確認。'
- バージョン別配置、ランチャー、インストーラー、アプリ内更新、チャンネル別データ、開発版更新を `777b7da` に、配布用 CI を `f4b72af` にコミットした。
- ホストで Linux 向けと `xwin` の Windows 向けの `cargo check` が通過した。`install-layout` の 4 件とアップデーターの 14 件のテストも通過した。
- Windows で `release/Test-Install.ps1` の全 14 件が通過した。`TEMP` を `Y:` に設定した場合も全件通過した。
- 一時環境で、ランチャーが `current` で選ばれた版を起動し、分離したデータディレクトリを使うことを確認した。従来データの beta への移行では、キャッシュを除外し、移行元を保持し、`.migrated-from` を作成した。
- アプリを起動したまま `scripts/dev-install.sh` を 2 回実行し、実行中の版がそのまま動き、次回起動で最新の開発版が選ばれることを確認した。
```

## 次の手順

```yaml
[]
```

## 試して駄目だった方法

```yaml
- approach: Velopack を第一候補にする
  reason: '`current` ディレクトリを使う構成で、`v<version>/` ごとに実行ファイルを置く要件に合わない。'
- approach: WSL で Windows 向けのリリース成果物を作る
  reason: '`gpui_windows` のリリースビルドがシェーダーの埋め込みに `fxc.exe` を必要とするため、GitHub Actions の Windows ジョブを使う。'
- approach: stable と beta でアプリデータを共有する
  reason: 新版がセッション形式を更新すると、旧版がその形式の読み込みを拒否し、書き込みを無効にするため。
```

## 確認すること

```yaml
null
```

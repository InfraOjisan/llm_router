# hispeed-router

Rust 製のローカルモデルルーターです。OpenAI 互換の `POST /v1/chat/completions` と `POST /v1/responses` を受け、JSON 設定に書いた API エンドポイントとモデルへ転送します。ルートの条件は英語または日本語の自然言語で指定できます。

最初のリクエストで、ローカルの Laya 互換 `/v1/systemone` 判定 API にルート候補を `choice` 質問として渡します。画像や PDF を含む入力は `multimodal_route` に直接送ります。判定 API が失敗、低信頼、未設定の場合は `fallback_route` を使います。同じ `x-harness-id` と `x-session-id` の組は、プロセスが動作している間、最初に選んだルートに固定されます。同時に到着した最初の複数リクエストも一度だけ判定します。

## 準備

Rust の stable ツールチェーンをインストールした環境で実行します。このリポジトリの開発環境には Rust ツールチェーンがないため、ここではビルドと実測値を確認できていません。

```powershell
Copy-Item config.example.json config.json
# config.json のモデル名、URL、必要な api_key_env を編集
cargo run --release -- --check config.json
cargo run --release -- config.json
```

Laya を使う場合は、[Laya の公式リポジトリ](https://github.com/NandhaKishorM/laya)に従ってローカルで `laya-serve` を起動し、`decision.url` をその `/v1/systemone` に向けます。`decision.model` に `multilingual` を指定すると日本語を含む入力向けチェックポイントを選べます。Ollama の OpenAI 互換 API は通常 `http://127.0.0.1:11434/v1` です。モデルのダウンロード、判定サーバー、Ollama や各転送先の起動は別途必要です。

## リクエスト

```powershell
$headers = @{ "x-harness-id" = "my-agent"; "x-session-id" = "conversation-123" }
$body = @{ model = "auto"; messages = @(@{ role = "user"; content = "この設計案の問題点を検討して" }); stream = $false } | ConvertTo-Json -Depth 10
Invoke-RestMethod -Uri http://127.0.0.1:8787/v1/chat/completions -Method Post -Headers $headers -ContentType application/json -Body $body
```

ルーターは入力の `model` を選択した転送先のモデル名に置き換えます。転送先の応答本文と HTTP ステータスを返し、`stream: true` の SSE もチャンク単位で転送します。応答ヘッダーの `x-router-route` は選ばれたルート、`x-router-source` は `decision`、`multimodal`、`fallback`、または `pinned` です。`GET /health` はヘルスチェックです。

## 設定

[`config.example.json`](config.example.json) に全フィールドの例があります。

- `endpoints`: 転送先名ごとに `base_url`、`model`、必要なら `api_key_env` を指定します。API キーの値は設定ファイルに書かず環境変数に置きます。
- `routes`: 判定モデルに渡す `id`、自然言語の `condition`、転送先を示す `endpoint` を指定します。複数ルートが同じ転送先を使うこともできます。
- `decision`: Laya/Jev 互換の `POST /v1/systemone` の URL、任意の `model`、`timeout_ms`、`min_confidence` を指定します。`min_confidence` は応答の `answer_confidence` に適用します。
- `multimodal_route`: 画像または PDF の入力を送るルートです。省略すると判定モデルに任せます。
- `fallback_route`: 判定できなかったときの経路です。品質が必要なタスクを取りこぼさないモデルに設定してください。
- `max_body_bytes`: 受け付ける JSON リクエスト本文の上限です。画像や PDF を埋め込む場合は必要に応じて増やします。

転送先は、使用するパスの OpenAI 互換リクエストを受けられる必要があります。ルーター自身は OCR を実行せず、ファイル形式を変換しません。PDF は転送先がその `input_file` などの形式を受け付ける場合に処理できます。

## 動作上の注意

- セッション固定情報はメモリ上に保存します。再起動後の同一経路は保証されず、長時間運転ではセッション数に応じてメモリが増えます。
- ルーティングの判定結果は実データで評価し、`condition` と `min_confidence` を調整してください。設定例の信頼度は初期値であり、精度の保証ではありません。
- 現在はホットリロードと転送先障害時の別モデルへの自動切替はありません。切替によって同一セッションのモデルが変わることを避けています。
- 転送先の認証には `api_key_env` を使用します。受信した `Authorization` ヘッダーは転送しません。デフォルトの待受先はループバックアドレスです。

## テスト

```powershell
cargo test
```

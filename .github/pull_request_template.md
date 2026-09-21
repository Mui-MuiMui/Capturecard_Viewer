<!--
マージ先は dev。main へ入れるのはリリースのときだけ。
書き方の決まりは .claude/skills/naming-conventions/SKILL.md の「PR タイトル」「PR 本文」を参照。
埋め終わったらこのコメントと各見出しのコメントは消す。
-->

## 対応する Issue

<!-- Closes / Fixes は使わない。実機確認が済む前に Issue が閉じるのを防ぐため、参照は全て Refs に揃えている。
     詳しい理由は .claude/skills/naming-conventions/SKILL.md の「Closes ではなく Refs を使う」を参照。 -->

Refs #

## 変更の要点

<!-- 何をどう変えたか。なぜそうしたかが分かると読みやすい。 -->

-

## 検証

<!-- .claude/skills/verify/SKILL.md の 4 段。通したものにチェックを入れる。 -->

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --locked --all-targets -- -D warnings`
- [ ] `cargo build --locked --release`
- [ ] `cargo test --locked`

実機確認: <!-- 済み / 未（実機が必要な変更なら未のまま出してよい。下の節に手順を書く） -->

## 人間が dev で確認すること

<!-- マージ後に人がなぞる操作手順と期待結果。実機依存でないなら「なし（ソース上の変更のみ）」と書く。 -->

-

## 判断を仰ぐ点

<!-- 迷った点、別案、積み残し。無ければこの節ごと消す。 -->

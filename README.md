# kiki-rss

`kiki` is an RSS/Atom feed _engine_. It is not a complete feed reader in and of
itself, but rather a reusable component that can be run behind the scenes to
power a reader.

## Example usage

Start a Kiki server with

```bash
cargo run --release -- init .
cargo run --release -- serve
```

You can add feeds to the server with e.g.

```bash
curl \
    --header 'Content-Type: application/json' \
    --data '{"title": "my feed", "url": "https://kernelmethod.org/notes/index.xml"}' \
    --unix-socket ./kiki.sock \
    http://localhost/v1/feeds/create
```

You should then be able to see the server listed with

```bash
curl \
    --unix-socket ./kiki.sock \
    http://localhost/v1/feeds

curl \
    --unix-socket ./kiki.sock \
    http://localhost/v1/feeds/id/$id
```

The server will automatically populate its database with feed entries once
you've added some endpoints. However, you can manually trigger a feed fetch
with

```bash
curl \
    --unix-socket ./kiki.sock \
    --request POST \
    http://localhost/v1/feeds/fetch/$id
```

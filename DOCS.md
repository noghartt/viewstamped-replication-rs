## Configuration file

```toml
[[replica]]
address = "127.0.0.1:8000"

[[replica]]
address = "127.0.0.1:8001"
```

## Questions

- How exactly should we manage `client-id`? Considering that it's an unique identifier should we, somehow, doesn't allow to let two clients having the same client-id?
  Also, didn't find any specific mention to duplicated client-id in the paper itself.
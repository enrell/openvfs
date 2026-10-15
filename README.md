# openvfs

**Sistema de arquivos virtual (VFS) em camadas para Rust.** Sem dependências externas, `#![forbid(unsafe_code)]`,
licença MIT.

O openvfs oferece uma única API de arquivos com vários tipos de armazenamento por trás, combináveis entre si: memória,
disco, montagens e camadas *copy-on-write* com rastreamento de mudanças. Todos os backends passam pela **mesma suíte
de conformidade e de testes diferenciais**, então o seu código se comporta igual onde quer que os bytes estejam.

```rust
use openvfs::prelude::*;

let mounts = MountFs::new();
mounts.mount("/", MemoryFs::with_quota(64 << 20))?;                       // raiz em RAM, até 64 MiB
mounts.mount("/assets", ReadOnlyFs::new(PhysicalFs::new("./assets")?))?;  // disco, somente leitura

let vfs = Vfs::new(mounts);
vfs.create_dir_all("/save/slot1")?;
vfs.write_atomic("/save/slot1/state.json", r#"{"level": 3}"#)?;          // nunca fica pela metade
let logo = vfs.read("/assets/logo.png")?;
```

> Implementação *clean-room*: escrita do zero, sem consultar o código de outros VFS.

## Usar em outros projetos

A biblioteca é o crate `openvfs` (sem dependências, MSRV 1.88). No `Cargo.toml` do seu projeto:

```toml
[dependencies]
openvfs = { path = "../openvfs/crates/openvfs" }   # ou { git = "https://github.com/enrell/openvfs" }
```

Depois de publicar no crates.io: `openvfs = "0.1"`. Escreva suas funções recebendo `&Vfs` (ou `&dyn FileSystem`):
o backend vira uma escolha de quem chama, e nos testes basta passar `Vfs::memory()`. O README do crate
([`crates/openvfs/README.md`](crates/openvfs/README.md)) traz um exemplo mínimo e os limites a conhecer.
`cargo package -p openvfs` já valida o pacote; o `openvfs-sandbox` é só uma aplicação (`publish = false`).

## Arquitetura

```
                ┌───────────────────────────────┐
  seu código ──▶│ Vfs  (API no estilo std::fs)  │  read, write_atomic, walk, copy_dir_all, try_exists…
                └──────────────┬────────────────┘
                               │ trait FileSystem: 7 primitivas + 2 com implementação padrão
       ┌───────────┬───────────┼────────────┬──────────────┐
   MemoryFs    PhysicalFs   ReadOnlyFs    MountFs       OverlayFs
   RAM, cota,  diretório    wrapper       tabela de     upper gravável sobre
   operações   confinado,                 montagem      lowers somente leitura,
   atômicas    política de                              changes / commit / reset
               links
```

| Componente | Garantias |
|---|---|
| [`VPath`](crates/openvfs/src/path.rs) | absoluto, normalizado, `..` para na raiz, comparação sensível a maiúsculas |
| [`VfsError`](crates/openvfs/src/error.rs) | variantes tipadas; **sempre informa o caminho que você passou**; converte para `io::Error` sem perda |
| [`MemoryFs`](crates/openvfs/src/memory.rs) | toda operação é atômica e linearizável; cota de bytes; handles com semântica de Unix (sobrevivem ao `unlink` e ao rename) |
| [`PhysicalFs`](crates/openvfs/src/physical.rs) | confinamento contra caminhos hostis, nomes de dispositivo do Windows e symlinks (ver [SECURITY.md](SECURITY.md)) |
| [`MountFs`](crates/openvfs/src/mount.rs) | vence o prefixo mais longo; os ancestrais das montagens são diretórios virtuais; erros reescritos no caminho externo |
| [`OverlayFs`](crates/openvfs/src/overlay.rs) | as camadas inferiores nunca são escritas; operações linearizáveis; copy-up atômico; `changes()` exato; `commit` idempotente |
| [`ops`](crates/openvfs/src/ops.rs) | `write_atomic`, `create_dir_all`, cópia recursiva e `Walk` (ordenado, preguiçoso, com `max_depth` e `filter_entry`) |

## O contrato

Documentado no trait [`FileSystem`](crates/openvfs/src/fs.rs) e verificado por testes, ponto a ponto:

- `create_dir` e `open(create)` exigem que o pai exista; `remove_dir` só remove diretório vazio; a raiz é intocável.
- `rename` substitui um arquivo no destino, recusa sobrescrever um diretório e recusa mover um diretório para dentro
  de si mesmo; renomear algo para o próprio nome não faz nada.
- `copy_file` recusa copiar um arquivo sobre ele mesmo (em vez de truncá-lo).
- Um caminho que atravessa um arquivo (`/arq.txt/x`) gera `NotADirectory`; um caminho inexistente gera `NotFound`.
- A ordem das verificações é a mesma em todos os backends, então o erro reportado também é o mesmo.

## Caso de uso: `openvfs-sandbox`

Edite um diretório real com prévia e desfazer garantido. O disco é a camada inferior do overlay, e todas as edições
ficam em memória até o `commit`.

```bash
cargo run -p openvfs-sandbox -- ./meu-projeto -c "replace fetch_user load_user --ext rs; diff"
```

```bash
cargo run -p openvfs-sandbox -- ./meu-projeto --apply -f renomear.txt
```

Use `-c`/`-f` para modo lote: sem `--apply` é só uma simulação; qualquer erro aborta sem tocar o disco. Sem argumentos,
abre um shell interativo. Os comandos são `ls tree cd cat find grep write append mkdir rm mv cp replace status diff commit discard`.

## Testes

```bash
cargo test --workspace
```

As suítes de integração formam um único binário (`tests/it`), o que deixa a compilação e a execução bem mais rápidas.
Para rodar só uma delas: `cargo test -p openvfs --test it conformance::` (ou `differential::`, `overlay::`, …).

São **764 testes** (unitários, de integração, doctests e de ponta a ponta do CLI), organizados em camadas:

| Suíte | O que prova |
|---|---|
| [`conformance`](crates/openvfs/tests/it/conformance.rs) | **56 regras do contrato × 11 configurações** (memória, disco, disco seguindo links, overlay sobre memória, overlay sobre disco, overlay com upper em disco, duas lowers com "iscas", overlay empilhado, montagem, montagem aninhada e um backend mínimo de terceiro que usa as implementações padrão) = 616 testes |
| [`differential`](crates/openvfs/tests/it/differential.rs) | históricos aleatórios aplicados a um `MemoryFs` de referência e a cada configuração: resultado, **variante do erro e caminho** precisam bater a cada passo; a árvore inteira é comparada periodicamente; as camadas inferiores precisam ficar intactas |
| [`overlay`](crates/openvfs/tests/it/overlay.rs) | semântica de copy-on-write e propriedades algébricas: `changes()` é exato e bem classificado; `commit` reproduz a visão mesclada; `reset` restaura a original |
| [`faults`](crates/openvfs/tests/it/faults.rs) | injeção de falhas em **cada ponto possível**, em dois modelos (disco morto e falha transitória): commit retomável, `write_atomic` nunca expõe dados parciais, copy-up e rename nunca corrompem a visão, rollback de rename de diretório |
| [`concurrency`](crates/openvfs/tests/it/concurrency.rs) | escritores em paralelo, `create_new` exclusivo, `write_atomic` nunca observado pela metade, appends não se perdem, renames atômicos para quem está listando, "tempestade" de mutações no overlay; tudo com watchdog contra deadlock |
| [`scaling`](crates/openvfs/tests/it/scaling.rs) | contratos de complexidade: listar um diretório não depende do tamanho da subárvore, resolver um caminho não depende do número de montagens; falham se algum deles voltar a ser linear |
| [`physical`](crates/openvfs/tests/it/physical.rs) | ataques de traversal, nomes de dispositivo, aliases de maiúsculas, symlinks e junctions (fuga, pendentes, loops), arquivos especiais |
| [`memory`](crates/openvfs/tests/it/memory.rs), [`mount`](crates/openvfs/tests/it/mount.rs), [`api`](crates/openvfs/tests/it/api.rs) | contabilidade da cota, semântica de handles, sombreamento de montagens, `Send`/`Sync` em tempo de compilação, mapeamento de erros e propriedades do `VPath` |
| [`cli`](crates/openvfs-sandbox/tests/cli.rs) | o binário real: códigos de saída, simulação que não altera o disco, `--apply`, scripts, REPL, saída sem ANSI em pipe |

**Os testes aleatórios são reproduzíveis** (PRNG com semente, sem dependências) e escaláveis por variáveis de
ambiente:

```bash
OPENVFS_DIFF_CASES=3000 OPENVFS_DIFF_STEPS=200 OPENVFS_PROP_CASES=3000 cargo test --release -p openvfs
```

```bash
OPENVFS_DIFF_SEED=1592590357 cargo test -p openvfs --test it differential::
```

`OPENVFS_DIFF_THREADS` controla quantas threads dividem os históricos de cada backend (padrão: núcleos, até 8).

Um teste preso (deadlock) não trava a execução: os testes concorrentes têm watchdog próprio e o processo inteiro aborta
após `OPENVFS_TEST_DEADLINE_SECS` segundos (padrão 180; aumente nas execuções com `OPENVFS_DIFF_CASES` grande).

A CI ([`.github/workflows/ci.yml`](.github/workflows/ci.yml)) roda fmt, clippy pedântico e rustdoc com
`-D warnings`, testes em Linux, macOS e Windows, a MSRV (1.88) e uma execução profunda noturna dos testes aleatórios.

## Desempenho

O custo de cada operação, com *n* nós, *c* filhos, *d* componentes no caminho e *m* montagens:

| Backend | Operação | Custo |
|---|---|---|
| `MemoryFs` | `metadata`, `open`, `create_dir`, `remove_*` de um nó | O(log n) |
| `MemoryFs` | `read_dir` | O(c · log n), **independe** do que há abaixo dos filhos |
| `MemoryFs` | `rename` / `remove_dir_all` de um diretório | O(nós na subárvore) |
| `MountFs` | resolver um caminho | O(d · log m), **independe** de quantas montagens existem |
| `OverlayFs` | leituras | concorrentes (lock de leitura); o custo é o da camada que responde |
| `OverlayFs` | operações estruturais (`rename`, `remove`, copy-up) | serializadas por um lock global |
| `PhysicalFs` | qualquer operação, `Symlinks::Deny` | ~*d* chamadas de sistema (uma `lstat` por componente) |
| `PhysicalFs` | qualquer operação, `FollowWithinRoot` | tipicamente uma `canonicalize` |

Os contratos em **negrito** são verificados por [`tests/it/scaling.rs`](crates/openvfs/tests/it/scaling.rs). Para números na
sua máquina (varia muito com o disco e com antivírus):

```bash
cargo run --release -p openvfs --example bench
```

Medido num Windows 11, uma máquina, ruído de até 2×: `MemoryFs::read_dir("/")` com dois filhos custava 18 µs com 1 mil
nós abaixo e **1,8 ms** com 100 mil; agora custa ~2–5 µs nos dois casos. `MountFs::metadata` com 1024 montagens caiu de
9 µs para ~1,7 µs, praticamente o custo de uma única montagem. No `PhysicalFs` o custo é dominado pelas chamadas de sistema:
nessa máquina cada uma custa ~16 µs, então `metadata` a 8 níveis de profundidade leva ~160 µs contra ~30 µs do
`std::fs`. Em Linux, onde uma chamada de sistema custa ~1 µs, a diferença é bem menor, mas isso ainda não foi medido.

## Limitações conhecidas

- Não é uma fronteira contra processos hostis *concorrentes* no host (TOCTOU). Ver [SECURITY.md](SECURITY.md).
- `PhysicalFs` com `Symlinks::Deny` paga uma chamada de sistema por componente do caminho; em árvores muito
  profundas e acessadas com frequência isso é o gargalo (ver [Desempenho](#desempenho)).
- `MemoryFs::rename` e `remove_dir_all` de um diretório custam proporcionalmente ao tamanho da subárvore.
- `read_dir` devolve um `Vec` completo; não há listagem em streaming nem API assíncrona.
- `OverlayFs::commit` é idempotente, mas não atômico; um `rename` entre montagens é copiar + apagar.
- Os whiteouts do overlay vivem em memória (`whiteouts()` permite persisti-los por fora).
- O modelo virtual não tem symlinks, permissões nem atributos estendidos.
- Hosts que não diferenciam maiúsculas (Windows, macOS) mantêm essa semântica dentro do `PhysicalFs`.

## Licença

[MIT](LICENSE).

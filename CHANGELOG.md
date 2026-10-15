# Changelog

Formato baseado em [Keep a Changelog](https://keepachangelog.com/pt-BR/1.1.0/); o projeto segue
[SemVer](https://semver.org/lang/pt-BR/).

## [0.1.0] — não publicado

Primeira versão sob o nome **openvfs** (antes era um protótipo chamado "strata").

### Adicionado
- Pacote pronto para uso como dependência: README e LICENSE dentro do crate, `cargo package` validado e testado a partir
  de um projeto externo; `openvfs-sandbox` marcado com `publish = false`.
- `VfsFile::set_len` e `VfsFile::sync_all`; `Vfs::set_len`.
- `Vfs::write_atomic` / `ops::write_atomic`: os leitores veem o conteúdo antigo ou o novo, nunca uma mistura.
- `Vfs::try_exists`, que distingue "não existe" de "não foi possível verificar".
- `FileSystem::remove_dir_all` e `FileSystem::copy_file`, com implementação padrão e versões atômicas ou nativas nos
  backends.
- `MemoryFs::with_quota`, `used_bytes`, `quota`.
- `PhysicalFs::builder` com política de links (`Symlinks::Deny` como padrão, ou `Symlinks::FollowWithinRoot`).
- `Walk::max_depth` e `Walk::filter_entry` (podam subárvores).
- `OverlayFs::whiteouts`.
- Erros `VfsError::PermissionDenied` e `VfsError::StorageFull`; `VfsError::kind()`.
- `Debug` para `dyn VfsFile`, `Borrow<str>` para `VPath`.

### Corrigido
- **Perda de dados:** `copy(a, a)` truncava o arquivo. Agora retorna `InvalidInput`.
- **Windows:** `CON`, `NUL.txt`, `COM1` etc. eram aceitos e a escrita ia para o dispositivo. Nomes terminados em `.`
  ou espaço criavam alias. Agora todos são rejeitados.
- **Overlay:** uma falha no copy-up (disco cheio, cota, E/S) deixava um arquivo parcial que sombreava o original.
- **Overlay:** renomear um arquivo da camada inferior por cima de outro apagava o destino antes de copiar.
- **Overlay:** `metadata().read_only` vinha `true` para arquivos que o overlay consegue escrever.
- **Mount:** `rename` entre montagens fazia *merge* com um diretório existente no destino.
- **Consistência:** o mesmo erro tinha variante e caminho diferentes conforme o backend. Agora há um contrato único,
  coberto pelas suítes de conformidade e diferencial.
- **Symlinks:** `walk` entrava em loop infinito com links circulares.

### Desempenho
- `MemoryFs::read_dir` custava O(subárvore) por listar todos os descendentes e filtrar os filhos: listar `/` com dois
  filhos levava 1,8 ms com 100 mil nós abaixo. Agora é O(filhos · log n) e não depende do tamanho das subárvores.
- `MountFs` varria a tabela inteira de montagens em quase toda operação; agora usa consultas de intervalo e o custo
  não depende do número de montagens (9 µs → ~1,7 µs com 1024 montagens).
- `PhysicalFs` com `Symlinks::Deny` reaproveita a metadata obtida na verificação de links (uma chamada de sistema a
  menos em `metadata` e `open`).
- `examples/bench.rs`: micro-benchmarks sem dependências para todos os backends.
- `tests/it/scaling.rs`: contratos de complexidade (listar um diretório não depende do tamanho da subárvore; resolver
  um caminho não depende do número de montagens).

### Tempo dos testes
- Os 10 arquivos de teste de integração viraram **um só binário** (`tests/it/main.rs`; rode uma suíte com
  `cargo test -p open-vfs --test it differential::`). Antes eram 10 executáveis: cada um recompilava `common/`, vinculava
  separado e pagava a varredura do antivírus na primeira execução. Medido no Windows: recompilar após editar o código
  caiu de ~17 s para ~2 s, as suítes passaram de ~8 s em sequência para ~2 s em paralelo, e o ciclo completo de
  `cargo test --workspace` de ~25 s para ~10 s.
- O teste diferencial reparte as históricos de cada backend entre threads (`OPENVFS_DIFF_THREADS`, padrão: núcleos, no
  máximo 8). A execução `deep` da CI, com 3000 históricos por backend, caiu de ~500 s para ~210 s; o padrão, de ~1,6 s
  para ~0,7 s. A primeira falha para as outras threads e é relançada com a mensagem original e a seed para replay.
- Como as suítes agora rodam juntas, um deadlock em qualquer teste (inclusive os sem watchdog próprio) segurava o
  binário inteiro. Agora há um prazo para o processo todo (`OPENVFS_TEST_DEADLINE_SECS`, padrão 180 s; a CI `deep` usa
  7200 s): ao estourar, o processo aborta com código 101 e uma mensagem apontando os testes suspeitos. O watchdog do
  diferencial, antes fixo em 1 h, passou a ser proporcional ao trabalho pedido (~2 min no padrão).

### Corrigido nos testes
- `atomic_writes_are_never_observed_torn` era instável (falhava em ~1 de 3 execuções e podia passar sem exercitar
  nada): o escritor terminava antes de as threads leitoras começarem. Agora ele só termina depois de um número
  mínimo de leituras que de fato se sobrepuseram às escritas.
- O teste de rename concorrente do overlay falha rápido, com a mensagem do observador, em vez de esperar o watchdog.

### Alterado
- O overlay ficou linearizável: as operações estruturais são serializadas.
- `commit` e `changes` fazem streaming do conteúdo em vez de carregar arquivos inteiros na memória.
- MSRV 1.88.

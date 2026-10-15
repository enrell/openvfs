# Segurança e modelo de ameaças

## O que o openvfs garante

**Caminhos não escapam da raiz.** Todo caminho passa por `VPath`, que normaliza a entrada e resolve `..`
lexicalmente, sempre parando na raiz. Os backends nunca recebem um `..`.

**`PhysicalFs` confina caminhos hostis à pasta-raiz:**

| Ataque | Defesa | Teste |
|---|---|---|
| `../../etc/passwd`, `\..\x` | `VPath` limita na raiz | `physical::traversal_never_leaves_the_root` |
| `C:\…`, `\\?\…` (prefixos) | componente precisa ser um único `Normal` | `physical::windows_device_names_and_aliases_are_rejected` |
| `CON`, `NUL.txt`, `COM1`, `LPT¹` (dispositivos Windows) | rejeitados com `InvalidPath` | idem + teste unitário `windows_name_rules` |
| `arquivo.txt:stream` (NTFS ADS), `<>:"\|?*` | rejeitados | idem |
| `nome.` / `nome ` (o Windows remove e cria um alias) | rejeitados | idem |
| symlink/junction apontando para fora | `Symlinks::Deny` (padrão): `PermissionDenied`; `FollowWithinRoot`: `PathEscapesRoot` | `physical::deny_policy_…`, `physical::follow_policy_…` |
| symlink pendente (criaria o arquivo fora) | tratado como fuga | idem |
| loop de symlinks (`loop -> .`) | `Deny`: invisível; `Follow`: use `Walk::max_depth` | idem |
| `remove_dir_all` em pasta que contém link | remove o link, nunca o alvo (`std::fs::remove_dir_all`) | `recursive_delete_removes_links_not_targets` |
| FIFO, socket, dispositivo dentro da raiz | invisíveis (abrir um FIFO bloquearia) | `special_files_and_unaddressable_names_are_invisible` (Unix) |

**Memória limitada para entradas não confiáveis.** `MemoryFs::with_quota(bytes)` limita o total de bytes. Isso vale
para escrita, append, `set_len`, cópia e escrita esparsa. Ao estourar a cota, a operação falha com `StorageFull`.

**Nenhuma perda silenciosa de dados.** Copiar um arquivo sobre ele mesmo é recusado; antes, isso o truncava. No
overlay, copy-up e rename gravam em um nome temporário e fazem `rename` atômico para o lugar, então uma falha nunca
deixa um arquivo parcial no caminho real.

## O que o openvfs **não** garante

- **TOCTOU no host.** O confinamento do `PhysicalFs` vale para *caminhos*. Um processo hostil e concorrente que troque
  pastas por symlinks entre a verificação e o uso pode vencer a corrida. Se esse é o seu modelo de ameaça, combine
  com isolamento do SO (containers, namespaces, AppContainer, contas sem privilégio).
- **Permissões e ACLs.** O modelo virtual não tem dono nem modo de acesso. `PhysicalFs` opera com os direitos do
  processo.
- **Atomicidade entre camadas.** `OverlayFs::commit` é idempotente, mas não atômico. `Vfs::rename` entre montagens é
  copiar + apagar.
- **Durabilidade de diretório.** `write_atomic` faz `sync_all` do arquivo, mas não do diretório pai.

## Reportando vulnerabilidades

Abra um relatório privado no repositório (Security Advisories), sem issue pública. Inclua um caso que reproduza o
problema; o ideal é um teste no formato de `crates/openvfs/tests/it/physical.rs`.

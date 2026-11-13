# rom — proposta e etapas

Documento de continuidade. O código e estas decisões valem mais do que o rascunho inicial da especificação. Escolhas fechadas estão em [decisions.md](decisions.md).

## Proposta

`rom` (Runtime Orchestrator for Microservices) é um orquestrador local para desenvolver dezenas de microsserviços sem deixar todos ligados.

O caso real é o monorepo `D:\TI\tex\dev\tex-backend-dev`: cerca de 38 serviços, quase todos Node.js 26 LTS, cada um com o próprio `node_modules`, portas fixas lidas de `process.env.PORT`, HTTP com WebSocket e SSE. Hoje sobem juntos via `nodemon` + `concurrently` (`npm start` e os `start:*`).

O que o rom faz:

- Sobe um serviço só quando alguém pede, junto com o que ele precisa.
- Desliga o que ficou ocioso.
- Um watcher só, no lugar de um nodemon por serviço.
- Cada serviço continua em processo, event loop e heap próprios. Nunca juntar dois serviços Node num único processo.

O que o rom não é: Docker, Kubernetes, ou um runtime compartilhado de V8.

Ambiente desta máquina: Windows nativo, sem distro Linux no WSL (só `docker-desktop`). Rust 1.97. Node 26.13. O primeiro alvo do open source é Windows. Linux e macOS ficam para depois; o código recusa compilar fora do Windows de propósito.

## O que já está pronto (etapa 1)

Commit `663beb7` em `main`. O push para `git@github.com:lucas-sachet/rom.git` não saiu: `Permission denied (publickey)`. A identidade deste repositório é `Lucas Sachet <lucasachet@gmail.com>` (não usar o e-mail global da máquina).

Entregue:

- Workspace Cargo: `apps/rom` (CLI e daemon) e `crates/rom-core`.
- `rom.yaml` validado: porta duplicada, dependência inexistente, ciclo, diretório ausente, profile apontando para serviço desconhecido.
- Daemon por projeto, pipe nomeado `\\.\pipe\rom-<hash>`, estado em `.rom/`.
- Job Object por serviço: parar mata a árvore; se o daemon morrer, os filhos morrem junto.
- Comandos: `init`, `check`, `start`, `stop`, `restart`, `status`, `logs`, `daemon`.
- `PORT` e `ROM_SERVICE` injetados. Serviço Node também recebe `NODE_COMPILE_CACHE` em `.rom/compile-cache/<serviço>`.
- Pronto = a porta aceita TCP. Se `health` estiver no yaml, o `/health` precisa responder 2xx. A maioria dos serviços reais não tem health.
- 14 testes (`cargo test --workspace`), incluindo processos Node de verdade. `cargo clippy --workspace --all-targets -- -D warnings` limpo.

O `rom.yaml` do tex **não** está neste repositório. Está em `D:\TI\tex\dev\tex-backend-dev\rom.yaml` (não commitado lá). `rom check` passou: 38 serviços, profiles iguais aos scripts `start:*`. Três serviços ficaram de fora:

- `financial-mcp` e `insight-service` declaram a porta 8034.
- `portal-admin-service` lê `PORT` e não tem padrão.
- `ai-core-service` não tem entrypoint de servidor.

TypeScript sobe com `node -r ts-node/register` (maintenance usa `--import tsx`). `notification-service` é `go run ./cmd/server`. `cte-service` usa `--env-file=env.local`. Não copiar segredo de `.env` para o yaml.

## Regras para quem for implementar

- Não criar as 8 crates da especificação antiga. Só extraia um crate quando o binário ficar difícil de ler.
- Não adicionar SQLite, Axum, Tower nem Hyper enquanto o gateway for TCP.
- `serde_yaml` está arquivado. O loader é `serde_yaml_ng`. Campos desconhecidos falham (`deny_unknown_fields`): não aceitar `watch` ou `sharing` até existirem.
- O daemon não pode ser lançado com `std::process::Command`. Ele herdaria o stdout de quem chamou o CLI e `rom start` não volta. O caminho certo é `crates/rom-core/src/detach.rs` (`CreateProcessW` com lista explícita de handles, cada handle uma vez só).
- `rom stop` de um conjunto não pode recusar a parada por causa de um dependente que também está no pedido. Regressão coberta em `stopping_a_whole_chain_in_one_request_needs_no_force`.
- Parar é imediato (Job Object). Drenagem com `CTRL_BREAK` só na etapa 5.
- Não criar pacote npm para os serviços.
- Não somar RSS e apresentar como memória física.
- Nesta máquina o Windows reserva TCP 50000–50059. Não usar essa faixa para porta interna.
- `CARGO_TARGET_DIR` pode apontar para o cache do Cursor, não para `target\` do repo. Achar o binário com `cargo metadata`.
- Um daemon já rodando não é substituído por um binário novo. `rom daemon stop` antes de testar de novo.
- Teste que mata processo de verdade: filtrar pelo PID do serviço. `*server.js*` também casa com o `tsserver.js` do editor.

## Etapas

Cada etapa termina com teste automatizado e um ensaio manual. Não começar a próxima com a anterior quebrada.

### Etapa 2 — Gateway TCP e subida sob demanda

O gateway escuta as portas públicas do yaml (`127.0.0.1` apenas). O serviço sobe numa porta interna livre, recebendo essa porta em `PORT`.

Fluxo: conexão na porta pública identifica o serviço; se estiver dormindo, sobe uma vez só (vários clientes simultâneos compartilham a mesma subida); espera a porta interna aceitar TCP; copia bytes nos dois sentidos. WebSocket e SSE passam sem código extra. Sem HTTP, sem gRPC como caso separado.

Ainda não desliga por ociosidade. Serviço dormindo continua dormindo até chegar conexão ou `rom start`.

Critério: três serviços de teste, todos dormindo; uma conexão no financeiro sobe o user e depois o financeiro; a conexão original é atendida; dez conexões juntas criam um processo só.

### Etapa 3 — Ociosidade

Conexão aberta (incluindo SSE e WebSocket) mantém o serviço ocupado. Keep-alive sem bytes nos dois sentidos é fechado depois de 5s (configurável) e não conta como atividade. Esgotado o idle, o gateway para de aceitar, espera as conexões restantes e para o serviço.

`mode: persistent` nunca para por ociosidade (fila, cron). O yaml ainda não tem esse campo; adicionar junto com a etapa.

Serviço `persistent` e serviço com consumidor ativo não param. Dependência declarada no yaml é dica de ordem (`start_before`), não a única forma de subir: chamada pela porta pública também liga o destino.

### Etapa 4 — Watcher no lugar do nodemon

Um watcher para o projeto (`notify`, mas sem registrar `node_modules` — no Windows o filtro também precisa acontecer antes de descer na árvore). Ignorar `node_modules`, `.git`, `dist`, `build`, `coverage`, `logs`, `.cache`, `.rom`.

Debounce e janela de estabilização configuráveis (ponto de partida 300ms e 500ms). Vários arquivos do mesmo serviço geram um restart. Serviço dormindo não sobe por causa de arquivo alterado. `git` com `index.lock` pausa o reload.

### Etapa 5 — Parada com CTRL_BREAK

Quando o gateway souber quais conexões estão abertas: console oculto por serviço, `CTRL_BREAK` (`SIGBREAK` no Node), espera `shutdown_timeout`, e só então `TerminateJobObject`. No Linux/macOS, o par será `SIGTERM` e depois `SIGKILL`. Sem cliente npm.

### Etapa 6 — Descoberta

Varredura do monorepo gera um yaml sugerido, que o desenvolvedor confirma. Não gravar por cima de um `rom.yaml` existente sem pedido. O registry continua sendo a verdade. Não inferir que código parecido é o mesmo processo.

### Etapa 7 — Métricas

PID, estado, memória, CPU, conexões ativas, tempo de subida, restarts, última atividade. Separar o custo do daemon do custo dos serviços. No Windows, não publicar soma de RSS como RAM real.

### Etapa 8 — Linux e macOS

Só depois do fluxo Windows estar estável. Trocar Job Object, pipe nomeado e `CTRL_BREAK` por process group, socket em `$XDG_RUNTIME_DIR` e `SIGTERM`. O gateway TCP e o yaml não mudam.

## Fora destas etapas

Dashboard, API de sessão de edição para o Cursor, SQLite e histórico. Rajada de edição já é tratada pelo debounce da etapa 4.

## Como retomar

```powershell
cd D:\TI\rom
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Ensaio no monorepo real, sem gateway (os serviços escutam a porta pública direto):

```powershell
cd D:\TI\tex\dev\tex-backend-dev
rom start --profile financial
rom status
rom stop --all
rom daemon stop
```

Não commitar `rom.yaml` nem `.rom/` no repositório do tex sem pedido. O `.gitignore` de lá já ignora `.rom/`.

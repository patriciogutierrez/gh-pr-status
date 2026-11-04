# PR Manager

App nativa de macOS para ver el estado de tus pull requests abiertos en GitHub, escrita con
[GPUI](https://www.gpui.rs/), el framework de UI acelerado por GPU de Zed. Agrupa tus PRs
según lo que esperan, sigue el pipeline de deploy que un bot comenta en cada PR y manda
notificaciones de macOS cuando un deploy avanza o falla y cuando te aprueban un PR.

```
┌──────────────────────────────────────────────────────┐
│ [Mis PRs (5)] [Mergeadas]   actualizado…     [Actua]│
│ 2 atención  1 listos  2 esperando  0 borradores      │
│ NECESITAN TU ATENCIÓN 2                              │
│ ● Registrar la redirección del formulario…        ⋯ │
│   (Arreglar CI: tests) payments #539 · Aprobado … 2 h│
│   ▸ [✗ Deploy falló] [pre-prod ✗] 🚨 Falló el read…  │
│ LISTOS PARA MERGE 1                                  │
│ ▌ …                                                  │
└──────────────────────────────────────────────────────┘
```

## Instalación

### 1. Requisitos

| Qué | Para qué | Cómo |
| --- | --- | --- |
| macOS 11 o superior | GPUI usa Metal | — |
| Xcode completo (no solo Command Line Tools) | GPUI compila sus shaders con `xcrun -sdk macosx metal` | App Store, luego `sudo xcode-select -s /Applications/Xcode.app` |
| Rust estable (probado con 1.92) | Compilar | `curl https://sh.rustup.rs -sSf \| sh` |
| GitHub CLI (`gh`) con sesión iniciada | Todos los datos salen de `gh api graphql` | `brew install gh` y `gh auth login` |
| `terminal-notifier` (opcional, recomendado) | Que el clic en una notificación traiga la app al frente | `brew install terminal-notifier` |

La app **no guarda ni maneja ningún token**: reutiliza la sesión de `gh`. Basta con que
`gh auth status` muestre una cuenta con el scope `repo` (para ver PRs de repos privados).

### 2. Configurar (opcional)

Sin configuración la app ya muestra tus PRs, sus checks, reviews y conflictos, y avisa
cuando te aprueban uno. Las variables siguientes solo activan el seguimiento del **pipeline
de deploy** (ver [El pipeline](#el-pipeline)):

| Variable | Qué es | Ejemplo |
| --- | --- | --- |
| `GH_PR_STATUS_BOT` | Login de GitHub del bot que comenta las corridas del pipeline. Sin ella, no se muestran pipelines ni avisos de deploy | `my-deploy-bot` |
| `GH_PR_STATUS_GCP_PREPROD` | Proyecto de GCP del ambiente pre-prod, tal como aparece en el link a los logs (`…?project=<id>`) | `acme-staging` |
| `GH_PR_STATUS_GCP_PROD` | Proyecto de GCP de prod | `acme-prod` |
| `GH_PR_STATUS_GCP_DEVELOPMENT` | Proyecto de GCP de development | `acme-dev` |
| `GH_PR_STATUS_SLACK_CHANNEL` | ID del canal de Slack donde inicias tu workflow (por ejemplo, pedir review). Agrega «Solicitar review» al menú de cada PR | `C0123456789` |
| `GH_PR_STATUS_CODE_DIR` | Dónde están clonados los repos que lee el agente de cada grupo. Por defecto, `~/code` | `~/code` |
| `GH_PR_STATUS_REVIEW_WEBHOOK` | URL del webhook del workflow de review de Slack (`hooks.slack.com/triggers/…`). Habilita el formulario «Solicitar review» | `https://hooks.slack.com/triggers/T…/…/…` |
| `GH_PR_STATUS_SLACK_USER` | Tu ID de usuario de Slack, que va como requester | `U0123456789` |
| `GH_PR_STATUS_REVIEWERS` | Los reviewers del formulario, como `Nombre:ID` separados por coma | `Ana:U01,Beto:U02` |
| `GH_PR_STATUS_SLACK_TOKEN` | Token de usuario de Slack (`xoxp-…`) con `channels:history` y `groups:history`, para ubicar el pedido de review y contar sus respuestas | `xoxp-…` |
| `GH_PR_STATUS_REVIEW_CHANNELS` | Los canales del formulario, igual; el primero queda preseleccionado | `team-prs:C0123456789` |
| `GH_PR_STATUS_SLACK_TEAM` | ID del workspace de Slack (`T…`, aparece en `app.slack.com/client/<id>/…`). Con él el canal se abre directo en la app de Slack; sin él pasa por el navegador | `T0123456789` |

Los proyectos de GCP solo sirven para saber en qué ambiente falló un deploy cuando el
mensaje de error no lo dice de otra forma; puedes omitirlos.

Ponlas en tu `~/.zshrc` (o expórtalas antes de compilar):

```sh
export GH_PR_STATUS_BOT=my-deploy-bot
export GH_PR_STATUS_GCP_PREPROD=acme-staging
export GH_PR_STATUS_GCP_PROD=acme-prod
export GH_PR_STATUS_GCP_DEVELOPMENT=acme-dev
export GH_PR_STATUS_SLACK_CHANNEL=C0123456789
export GH_PR_STATUS_SLACK_TEAM=T0123456789
```

### 3. Instalar como app de macOS

```sh
git clone https://github.com/patriciogutierrez/gh-pr-status.git
cd gh-pr-status
scripts/bundle.sh --install   # compila en release y deja "PR Manager.app" en ~/Applications
```

Ábrela desde Spotlight, Finder o el Dock. La primera compilación de GPUI demora varios
minutos.

Una app abierta desde Finder **no hereda las variables de tu shell**, así que
`scripts/bundle.sh` copia las `GH_PR_STATUS_*` que estén definidas al empaquetar dentro del
`Info.plist` de la app (`LSEnvironment`). Si cambias alguna, vuelve a correr
`scripts/bundle.sh --install` y reinicia la app.

El script también firma la app ad-hoc, suficiente para una app compilada y usada en el
mismo Mac. Con `--install` la app se mueve a `~/Applications`, así Spotlight encuentra una sola
copia; sin `--install` queda en `target/release/bundle/`. El ícono sale de `assets/AppIcon.icns`;
`scripts/icon.sh` lo regenera a partir del dibujo en `scripts/icon.swift`.

### 4. Permitir las notificaciones

La primera notificación hace que macOS pida permiso. Si no aparecen, actívalas en
Configuración del Sistema → Notificaciones, para `terminal-notifier` (o para Script Editor
si no lo instalaste).

### Desde la terminal

```sh
cargo run
```

Así la app sí hereda las variables del shell. El clic en una notificación abre el PR en el
navegador en vez de traer la app al frente, para no lanzar la copia instalada.

## Qué muestra

- **Mis PRs** agrupa tus PRs abiertos según de quién depende el siguiente paso:
  - **Necesitan tu atención:** CI rojo, cambios pedidos, conflictos o un deploy que falló.
  - **Listos para merge:** aprobado, CI verde y sin conflictos.
  - **Esperando CI o review:** CI corriendo, falta review, o GitHub todavía no calcula si hay conflictos.
  - **Borradores.**
- **Mergeadas** lista tus PRs mergeados agrupados por día (Hoy, Ayer, «martes 13 oct»…), con
  la hora del merge. El registro parte el lunes de la semana pasada y se guarda en
  `~/Library/Application Support/gh-pr-status/merged.sqlite`: cada 5 minutos la app solo pide
  a GitHub lo mergeado desde la última sincronización (normalmente, hoy). Si la búsqueda de
  GitHub devuelve menos resultados de los que cuenta, guarda los que llegaron y vuelve a pedir
  los mismos días en la siguiente. Muestra los últimos 60 días; los anteriores quedan en el
  archivo. Borrarlo reinicia el registro.
- **Grupos**, en la barra izquierda, para ordenar los PRs cuando son muchos. Arrastra una fila
  (abierta o mergeada) a un grupo para moverla ahí, a «Sin grupo» para sacarla, o a
  «+ Nuevo grupo» para crear uno con ella adentro. Al elegir un grupo, las dos pestañas
  muestran solo sus PRs. Clic derecho sobre un grupo para renombrarlo o eliminarlo
  (sus PRs vuelven a «Sin grupo»). Un PR está en un solo grupo y se recuerda por su URL, así
  que sigue ahí después de mergearse. Arrastra la línea entre la barra y los PRs para
  cambiar su ancho, que se recuerda al reiniciar. Se guardan en
  `~/Library/Application Support/gh-pr-status/groups.sqlite`. El nombre se escribe sin un
  campo de texto nativo: los acentos con tecla muerta (´ + a) no se componen.
- **Subgrupos**, un nivel bajo cada grupo: clic derecho sobre el grupo → «Nuevo subgrupo».
  Al elegir el grupo ves sus PRs y los de todos sus subgrupos; al elegir un subgrupo, solo los
  suyos. La flecha ▸/▾ a la izquierda de un grupo pliega sus subgrupos, y queda así al
  reiniciar; un grupo plegado se marca como elegido cuando uno de sus subgrupos lo está.
  Eliminar un grupo elimina también sus subgrupos. En «Mis PRs», un grupo con
  subgrupos se ve como tablero: una columna por subgrupo (y «Sin subgrupo» primero, si el
  grupo tiene PRs propios) con tarjetas compactas ordenadas por estado. Arrastra una tarjeta a
  otra columna para cambiarla de subgrupo. Las columnas se desplazan hacia el lado con el
  trackpad o con shift+rueda, arrastrando el espacio vacío del tablero, y con la barra que
  aparece abajo al pasar el mouse, cuyo pulgar se puede arrastrar.
- **Un agente por grupo**, en [Orca](https://github.com/stablyai/orca). Con un grupo elegido,
  el botón **Agente** (o clic derecho → «Abrir agente en Orca») abre una pestaña de Orca con
  `claude --add-dir ~/code`, para conversar qué hay que hacer y en qué repos. La primera vez
  crea la carpeta del grupo en `~/orca/groups/<grupo>/`, un repo git con un `CLAUDE.md` que
  le explica al agente cómo trabajar, y la registra en Orca. Las siguientes vuelve a la misma
  pestaña. El agente crea un worktree de Orca por repo, marcado con el grupo
  (`--comment gh-pr-status:group=<id>`), y los PRs que salgan de esas ramas entran solos al
  grupo. Si sacas uno a mano, no vuelve a entrar. Los subgrupos comparten el agente de su
  grupo: la app mantiene en la carpeta del grupo un `subgrupos.md` con la marca de cada uno,
  y el agente marca cada worktree con el subgrupo al que pertenece, así sus PRs caen ahí. Los repos se buscan en `~/code`, o en
  `GH_PR_STATUS_CODE_DIR` si está definida. La primera vez, Claude Code pregunta si confías
  en la carpeta del grupo. Al eliminar un grupo, su carpeta queda en disco y registrada en
  Orca; sácala desde Orca.
- Cada fila lleva en color su **siguiente acción**: lo primero que la traba (deploy fallido,
  conflictos, CI roja, cambios pedidos), si no lo que espera (CI, review de alguien, GitHub) y
  si no «Mergear». Es el único rótulo con color de la fila. Cuando es algo que un agente puede
  hacer, el menú del PR ofrece **Pedir al agente**: abre el agente del grupo del PR y le
  escribe la tarea con el PR, la rama, los checks fallidos y sus logs, y cómo ubicar o crear el
  worktree. El PR tiene que estar en un grupo; si el agente recién abrió y pregunta si confías
  en la carpeta, hay que responderle en Orca y pedir de nuevo.
- **Hilos de review sin resolver**: la fila dice «▸ N hilos sin resolver» y al desplegarlos
  muestra quién los abrió, en qué archivo y línea, y el comienzo del comentario (sin la
  metadata de los bots); el clic abre el hilo en GitHub. Con hilos abiertos, la siguiente acción
  pasa a «Responder N hilos de review» y «Pedir al agente» le manda cada hilo con su link.
  Solo cuentan los hilos sobre código vigente (GitHub marca los demás como desactualizados).
- **Stacks**: si la base de un PR es la rama de otro PR tuyo abierto, la fila muestra la
  cadena completa (`master ← #7684 ← [#7685]`) y la siguiente acción pasa a «Esperando el
  merge de #7684»; solo el de abajo dice «Mergear». Cuando GitHub re-apunta un PR a otra base
  (al mergearse el de abajo), llega una notificación: es el momento de sacarlo de borrador.
- **Gemelas [MASTER]/[DEV]**: un PR con el mismo título bajo el otro prefijo, en el mismo
  repo, aparece en la fila como «Gemela [DEV] #N» con su CI y review. Solo entre PRs abiertos:
  una gemela cerrada sin mergear no se ve.
- Cada fila es el título del PR, con un punto del color de su sección, y debajo la acción
  seguida de una línea gris con repo y número, CI, review (quién aprobó o a quién se le
  pidió), conflictos, comentarios, la rama y hace cuánto se actualizó. Si el CI falla, el
  nombre del check va en la acción y sus logs en el menú del PR.
- Si configuraste el bot, debajo de cada PR tuyo está su **último pipeline**: estado
  (desplegando, desplegado, falló, sin avances), la etapa actual o el primer error, y hace
  cuánto escribió el bot. **Clic en el pipeline** lo despliega: últimas 8 etapas con su
  hora, todos los errores (servicio y causa) y un botón para abrir el comentario.
- **Clic en el título** abre el PR en el navegador.
- **El menú del PR**, con el «⋯» que aparece al pasar el mouse o con clic derecho, reúne sus
  acciones, en este orden: copiar la URL, ver el pedido en Slack, `/deploy`, solicitar review,
  marcar un borrador listo para review (`gh pr ready`), pedir al agente (salvo cuando lo que
  falta es responder hilos de review), ver los logs de cada check fallido (menos docs-check) y
  sacarlo del grupo.
- **Solicitar review** abre un formulario en una ventana aparte: reviewer (de la lista de
  `GH_PR_STATUS_REVIEWERS`; al abrir la lista se puede escribir para filtrarla, sin importar
  mayúsculas ni tildes, y enter elige la primera coincidencia), urgencia (highest, high,
  medium, low), canal, comentario, y el
  link del PR ya puesto. «Enviar a Slack» (o enter) hace un `POST` al webhook del workflow de
  review del equipo, que publica el pedido con su botón «aprobar»; tú vas como requester. La
  fila dice «review pedido hace N» hasta que el PR quede aprobado, y el menú deja de ofrecer
  el pedido; se recuerda en `reviews.sqlite`, junto a los otros archivos locales. Con
  `GH_PR_STATUS_SLACK_TOKEN` (un token de usuario con `channels:history` y `groups:history`),
  la app busca unos segundos después el mensaje que publicó el workflow, y desde ahí «Ver
  pedido en Slack» abre el hilo exacto, la fila suma «N respuestas en Slack» y cada respuesta
  nueva llega como notificación de macOS. Sin token, «Ver pedido en Slack» abre el canal. Necesita `GH_PR_STATUS_REVIEW_WEBHOOK`, `GH_PR_STATUS_SLACK_USER`,
  `GH_PR_STATUS_REVIEWERS` y `GH_PR_STATUS_REVIEW_CHANNELS`; sin ellas, y con
  `GH_PR_STATUS_SLACK_CHANNEL`, el menú vuelve a copiar el link y abrir el canal. El
  comentario se escribe sin campo nativo: los acentos con tecla muerta no se componen.
- **/deploy** comenta `/deploy` en el PR, como tu usuario de `gh`, para iniciar el
  pipeline. Pide un segundo clic de confirmación (dentro de 5 s; el menú se queda abierto) y
  no aparece en borradores ni mientras hay un deploy en curso.
- Las listas se recargan solas cada minuto; mientras alguno de tus PRs está desplegando,
  "Mis PRs" se recarga cada 20 s.

| Tecla | Qué hace |
| --- | --- |
| `cmd-r` | actualiza |
| `cmd-1` | pestaña Mis PRs |
| `cmd-2` | pestaña Mergeadas |
| `cmd-q` | sale |

### Notificaciones

Solo avisan los cambios que ocurren con la app abierta: la primera carga marca el punto
de partida, así que abrir la app no repite corridas ni aprobaciones viejas. Avisan:

- cuando un PR tuyo pasa a "Aprobado" (si el repo exige varias aprobaciones, cuando se
  cumplen todas);
- con el bot configurado, cada hito de un pipeline, una vez: `✓ Pre-prod listo`,
  `✓ Desplegado en prod` (o en development), `✗ Deploy falló en pre-prod`/`prod`,
  `🐦 Canary activo en prod` y `✗ Pipeline detenido` (por ejemplo, faltan approves).

Con `terminal-notifier` el clic trae la app al frente. Sin él se usan notificaciones de
`osascript`, que macOS atribuye a Script Editor: el clic abre Script Editor y no hay forma
de cambiarlo.

## El pipeline

La app entiende un formato de comentario concreto: el bot deja **un comentario por corrida
y lo va editando**. El comentario empieza con un encabezado que dice `Iniciando pipeline`,
y cada etapa agrega un encabezado `## <emoji> <etapa>` seguido de
`> 2026-10-05 14:58:26 GMT-03` y un cuerpo corto. La app toma el comentario de corrida
editado más recientemente entre los últimos 10 del PR, e ignora los otros comentarios del bot.

Una corrida normal despliega **pre-prod y después prod**; cada ambiente toma tráfico, corre
los sintéticos y cierra con `🌟 CD: OK!`. La corrida termina con
`🌟 CD: 🚀 El despliegue fue aplicado correctamente`. Cada etapa se asigna a su ambiente:

- por su título (`Info pre-prod: …`, `El ambiente prod ya recibe tráfico`, `Logs de prod`);
- si es una falla, por su cuerpo: `🌎 Ambiente: development` o el proyecto de GCP en el link
  a los logs (las variables `GH_PR_STATUS_GCP_*`);
- si no dice nada, por el ambiente que acaba de empezar a recibir tráfico.

| Estado | Cuándo |
| --- | --- |
| ✗ Deploy falló | hay un error (`🚨`, `❌`, `Error`, `Falló`) que la corrida no superó. Un `🔒 Bloqueo activo` que se libera y sigue no cuenta |
| ✓ Desplegado | aparece el cierre: despliegue o hotfix aplicado, pipeline a development terminado, canary activo, o "solo hay cambios en archivos ignorados" |
| ● Desplegando | ninguna de las anteriores y el bot escribió hace menos de 30 min |
| ◌ Sin avances | ninguna de las anteriores y el bot lleva más de 30 min sin escribir |

Si tu bot usa otro formato, el parser está en `src/pipeline.rs`.

## Cuánto cuesta el polling

Cada recarga de tus PRs es una llamada GraphQL de 2 puntos (pide los últimos comentarios),
de un presupuesto de 5.000 puntos por hora que comparten todas tus herramientas que usan `gh`. La de mergeadas cuesta 1 punto cada 5
minutos (~12 por hora).

| Situación | Puntos por hora | % del límite |
| --- | --- | --- |
| Sin despliegues (todo cada 60 s) | ~130 | ~3 % |
| Con un despliegue en curso ("Mis PRs" cada 20 s) | ~370 | ~7 % |

El estado de review sale de `reviewDecision`, que solo existe cuando el branch protection
exige reviews; si no, la app usa la última review de cada revisor.

## Problemas comunes

| Síntoma | Causa |
| --- | --- |
| "gh: command not found" o error de autenticación | Falta `brew install gh` o `gh auth login`. La app busca `gh` en el `PATH` y en `/opt/homebrew/bin` y `/usr/local/bin` |
| No aparecen pipelines | `GH_PR_STATUS_BOT` no está definida, o no lo estaba al correr `scripts/bundle.sh` |
| El clic en la notificación abre Script Editor | Falta `brew install terminal-notifier` |
| Error de compilación sobre `metal` | Falta Xcode completo o `xcode-select` apunta a Command Line Tools |

## Desarrollo

```sh
cargo test                          # el test que va a GitHub se salta solo
GH_PR_STATUS_LIVE=1 cargo test      # ese test sí hace la búsqueda real
cargo clippy --all-targets
```

| Archivo | Contenido |
| --- | --- |
| `src/main.rs` | La vista `Root`: encabezado, contadores, secciones, filas y recarga periódica |
| `src/github.rs` | La consulta GraphQL vía `gh`, el parseo de la respuesta y las fechas |
| `src/pipeline.rs` | La configuración del bot y el parseo de sus comentarios: etapas, horas, errores y estado |
| `src/folders.rs` | Los grupos de PRs y a cuál pertenece cada uno, en SQLite |
| `src/orca.rs` | El agente de cada grupo en Orca y los worktrees marcados con su grupo |
| `src/db.rs` | Dónde se abren los archivos SQLite locales |
| `src/merged.rs` | Los PRs mergeados: la búsqueda en GitHub, el registro en SQLite y los días en hora local |
| `src/notify.rs` | Detecta hitos de deploy y PRs aprobados, y manda la notificación de macOS |
| `src/tool.rs` | Encuentra `gh` y los notificadores aunque la app no herede el `PATH` del shell |
| `src/review.rs` | El pedido de review por el webhook del workflow de Slack y su configuración |
| `src/review_view.rs` | La ventana del formulario de review |
| `src/slack.rs` | Lectura de Slack con el token de usuario: el mensaje del pedido y sus respuestas |
| `src/stack.rs` | Stacks de PRs (base sobre la rama de otro) y gemelas [MASTER]/[DEV] |
| `src/status.rs` | En qué grupo cae un PR, sus badges y el "hace N min" |
| `src/ui.rs` | Botones, pestañas, el pill de la acción y los badges del pipeline |
| `src/theme.rs` | Colores (Catppuccin Mocha) |
| `scripts/bundle.sh` | Compila en release y arma el `.app` |

## Licencia

[MIT](LICENSE)

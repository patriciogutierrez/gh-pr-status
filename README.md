# gh-pr-status

App nativa de macOS para ver el estado de tus pull requests abiertos en GitHub, escrita con
[GPUI](https://www.gpui.rs/), el framework de UI acelerado por GPU de Zed. Agrupa tus PRs
según lo que esperan, sigue el pipeline de deploy que un bot comenta en cada PR y manda
notificaciones de macOS cuando un deploy avanza o falla y cuando te aprueban un PR.

```
┌──────────────────────────────────────────────────────┐
│ Pull requests                          [Actualizar]  │
│ @octocat · actualizado hace 2 min                    │
│ [Mis PRs (5)] [Me piden review (3)]                  │
│ ┌──────────┐┌──────────┐┌──────────┐┌──────────┐     │
│ │ 2        ││ 1        ││ 2        ││ 0        │     │
│ │ Atención ││ Listos   ││ Esperando││ Borrador │     │
│ └──────────┘└──────────┘└──────────┘└──────────┘     │
│ NECESITAN TU ATENCIÓN 2                              │
│ ▌ payments #539 · master ← feat/…        hace 2 h    │
│ ▌ Registrar la redirección del formulario…           │
│ ▌ [✗ CI] [Aprobado] [💬 2]  falla: tests             │
│ ▌ ▸ [✗ Deploy falló] [pre-prod ✗] 🚨 Falló el read…  │
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
| `GH_PR_STATUS_SLACK_CHANNEL` | ID del canal de Slack donde inicias tu workflow (por ejemplo, pedir review). Agrega un botón "Slack" al lado de cada PR | `C0123456789` |
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
scripts/bundle.sh --install   # compila en release y deja "GH PR Status.app" en ~/Applications
```

Ábrela desde Spotlight, Finder o el Dock. La primera compilación de GPUI demora varios
minutos.

Una app abierta desde Finder **no hereda las variables de tu shell**, así que
`scripts/bundle.sh` copia las `GH_PR_STATUS_*` que estén definidas al empaquetar dentro del
`Info.plist` de la app (`LSEnvironment`). Si cambias alguna, vuelve a correr
`scripts/bundle.sh --install` y reinicia la app.

El script también firma la app ad-hoc, suficiente para una app compilada y usada en el
mismo Mac. Con `--install` la app se mueve a `~/Applications`, así Spotlight encuentra una sola
copia; sin `--install` queda en `target/release/bundle/`.

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
- **Me piden review** lista los PRs abiertos donde alguien pidió tu review, con el autor.
- Cada fila muestra repo, número, rama base y rama head, la última actualización, y
  badges de CI, review y conflictos. Si el CI falla, muestra qué checks fallaron.
- Si configuraste el bot, debajo de cada PR tuyo está su **último pipeline**: estado
  (desplegando, desplegado, falló, sin avances), la etapa actual o el primer error, y hace
  cuánto escribió el bot. **Clic en el pipeline** lo despliega: últimas 8 etapas con su
  hora, todos los errores (servicio y causa) y un botón para abrir el comentario.
- **Clic en el PR** lo abre en el navegador.
- **Clic derecho en el PR** abre un menú para copiar su URL.
- Con `GH_PR_STATUS_SLACK_CHANNEL` definida, el botón **Slack** al lado de cada PR tuyo copia
  el link del PR y abre ese canal en Slack. Los workflows de Slack solo se inician desde dentro
  de Slack, así que desde ahí lo inicias (marcador del canal o menú `+`) y pegas el link. El
  botón desaparece cuando el PR queda aprobado.
- El botón **/deploy** comenta `/deploy` en el PR, como tu usuario de `gh`, para iniciar el
  pipeline. Pide un segundo clic de confirmación (dentro de 5 s) y no aparece en borradores
  ni mientras hay un deploy en curso.
- Las listas se recargan solas cada minuto; mientras alguno de tus PRs está desplegando,
  "Mis PRs" se recarga cada 20 s.

| Tecla | Qué hace |
| --- | --- |
| `cmd-r` | actualiza |
| `cmd-1` | pestaña Mis PRs |
| `cmd-2` | pestaña Me piden review |
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

Cada recarga es una llamada GraphQL por pestaña: 2 puntos la de tus PRs (pide los últimos
comentarios, solo de tus PRs) y 1 la de reviews, de un presupuesto de 5.000 puntos por hora
que comparten todas tus herramientas que usan `gh`.

| Situación | Puntos por hora | % del límite |
| --- | --- | --- |
| Sin despliegues (todo cada 60 s) | ~180 | ~4 % |
| Con un despliegue en curso ("Mis PRs" cada 20 s) | ~420 | ~8 % |

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
| `src/main.rs` | La vista `Root`: pestañas, resumen, secciones, filas y recarga periódica |
| `src/github.rs` | La consulta GraphQL vía `gh`, el parseo de la respuesta y las fechas |
| `src/pipeline.rs` | La configuración del bot y el parseo de sus comentarios: etapas, horas, errores y estado |
| `src/notify.rs` | Detecta hitos de deploy y PRs aprobados, y manda la notificación de macOS |
| `src/tool.rs` | Encuentra `gh` y los notificadores aunque la app no herede el `PATH` del shell |
| `src/status.rs` | En qué grupo cae un PR, sus badges y el "hace N min" |
| `src/ui.rs` | Botones, pestañas, badges y contadores compartidos |
| `src/theme.rs` | Colores (Catppuccin Mocha) |
| `scripts/bundle.sh` | Compila en release y arma el `.app` |

## Licencia

[MIT](LICENSE)

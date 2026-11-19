# Tests backend — session du 19 novembre 2026

Compte rendu des contrôles lancés sur la machine locale, branche `dev`, dépôt `~/Documents/Matsuo_deploy_KC/ninesql`.

Machine : Ubuntu 22.04.5 LTS, XPS 13, Intel Iris Xe (Tiger Lake GT2), Vulkan 1.3.255 (Mesa), session Wayland. Docker Compose v5.5.1, l’utilisateur est dans le groupe `docker`. Disque : environ 177 Go libres (81 % utilisés), au-dessus du seuil de 5 % exigé par YugabyteDB.

## Environnement

| Élément | Constat |
|---|---|
| Rust | Absent au départ. Installé avec rustup pendant la session. `rust-toolchain.toml` sélectionne le canal `stable` (rustfmt et clippy). |
| Python du système | `/usr/bin/python3` reste en 3.10.12. |
| Python pour `make check` | `scripts/verify-patches.sh` exige Python 3.11 (`tomllib`). Le paquet Ubuntu était la release candidate `3.11.0~rc1`. Le passage à deadsnakes `3.11.18` a d’abord échoué : `libpython3.11-stdlib` voulait écraser `/usr/lib/python3.11/sre_compile.py`, encore attribué à l’ancien `libpython3.11-minimal`. `sudo apt --fix-broken install` a terminé la mise à jour. `import tomllib` réussit. Le lien `~/.local/bin/python3` pointe vers le 3.11, sans remplacer le Python du système. |
| `cargo-deny` | 0.20.2, la version de la CI. |
| `dbus-daemon` | Installé, pour que `NINESQL_REQUIRE_DBUS=1` ne saute pas les tests du coffre. |

## `make check`

Commande : `NINESQL_REQUIRE_DBUS=1 make check TIMEOUT=3600`.

Résultat : **réussi**. Dernière ligne : `check: all passed`.

| Étape | Résultat |
|---|---|
| Format, clippy, tests sans moteur | Passés (les tests d’intégration sautent les moteurs injoignables). |
| `cargo deny` | `advisories ok, bans ok, licenses ok, sources ok`. Des avertissements `warning[duplicate]` listent deux versions de certaines crates (`thiserror`, `toml`, `toml_edit`, `winnow`, `zbus`, `zbus_macros`). La politique les accepte. |
| `verify-patches` | `xim-ctext` 0.3.0 et `zed-xim` 0.4.0-zed correspondent à leurs archives crates.io. |
| API GPUI interdites | Aucune sous `crates/`. |
| Liens Markdown | 0 lien cassé. |
| `dev-check` | 20 moteurs, 30 services, images épinglées, ports sur `127.0.0.1`. |

## `make test-engines`

Commande : `make test-engines TIMEOUT=3600 DEV_TIMEOUT=1800`.

Les moteurs ont démarré.

| Profil | Moteurs | Délai annoncé |
|---|---|---|
| `core` | PostgreSQL, MariaDB, MySQL | prêt en 16 s |
| `sql` (moteurs wire) | TiDB, QuestDB, TimescaleDB, YugabyteDB | prêt en 47 s |

La compilation des tests a pris 2 min 14 s. La grande majorité des tests de `ninesql-core`, `ninesql-driver-pgwire`, `ninesql-driver-mysqlwire`, `ninesql-driver-sqlite` et `ninesql-ssh` a réussi, y compris l’édition et le transfert sur PostgreSQL, MySQL, MariaDB et YugabyteDB.

**Échec : 2 tests sur 20** dans `ninesql-driver-mysqlwire`, binaire `tests/mysql.rs`.

| Test | Message |
|---|---|
| `edits_parameters_counts_notices_and_result_sets` | `CREATE FUNCTION ninesql_write_one …` : `FUNCTION ninesql_write_one already exists` (SQLSTATE 42000), à `tests/mysql.rs` dans `fixtures`. |
| `one_shot_read_write_transaction_is_reset` | `FUNCTION nine.ninesql_write_one does not exist` (code 1305), lors de `SELECT ninesql_write_one()` après `SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ WRITE`. |

Les deux tests appellent `fixtures()`, qui enchaîne `DROP FUNCTION IF EXISTS ninesql_write_one` et `CREATE FUNCTION` sur la base partagée `nine`. Cargo lance les tests d’un même binaire en parallèle. Un test recrée la fonction pendant que l’autre la supprime. Ce n’est pas un échec de Docker ni du moteur : PostgreSQL, MariaDB, TiDB, QuestDB, TimescaleDB et YugabyteDB étaient prêts, et les 18 autres tests de ce binaire ont réussi.

`make` a ensuite arrêté les conteneurs et effacé les volumes (`dev-down`), puis s’est terminé par l’erreur 101.

## Rejeu en série

Moteurs relancés (`make dev-up` et le profil `sql` pour TimescaleDB, YugabyteDB, QuestDB, TiDB), puis :

```bash
NINESQL_REQUIRE_ENGINES=postgresql,mysql,mariadb,ssh,timescaledb,yugabytedb,questdb,tidb \
  cargo test --locked -p ninesql-driver-mysqlwire --test mysql -- --test-threads=1
```

Résultat : **20 passed, 0 failed**, en 21,55 s. Les deux tests en échec passent quand ils ne se croisent plus.

Le `make test-engines` complet n’a pas été relancé après ce rejeu. Seul le binaire MySQL a été rejoué.

## Pas encore fait

- Paquet `.deb` : `make deb`, `make deb-reproducible`, `make deb-install-test`.
- Recherche du mot de passe en clair sur le disque (scénario I1).
- Mesures GPU (`crates/ninesql-ui/examples/spike-measure.sh` et le protocole de `crates/ninesql-grid/README.md`).

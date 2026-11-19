# Tests d’interface — session du 19 novembre 2026

Passe visuelle partielle, sur la même machine que [les tests backend](../test-backend/rapport.md). Session Wayland, thème clair, échelle 200 %. Binaire de développement : `make run` a compilé le profil `dev` en 1 min 26 s, puis a lancé `target/debug/ninesql`.

Une seule fenêtre NineSQL. Les bases de test du profil `core` étaient celles de `make dev-up` (PostgreSQL sur `127.0.0.1:15432`, utilisateur `nine`, base `nine`).

## Premier démarrage

La fenêtre s’ouvre. Le navigateur indique qu’aucune connexion n’existe. Une invite propose d’importer un workspace DBeaver Snap trouvé ici :

`/home/funuser/snap/dbeaver-ce/current/.local/share/DBeaverData/workspace6` (1 projet).

L’invite a été fermée sans import. Les fichiers DBeaver n’ont pas été modifiés. L’import lui-même n’a pas été refait ensuite.

## Connexion PostgreSQL

Dialogue **New connection**, connexion nommée `postgresql-test`.

| Champ | Valeur retenue |
|---|---|
| Moteur | PostgreSQL |
| Hôte / port | `127.0.0.1` / `15432` |
| Base | `nine` |
| Utilisateur | `nine` |
| Mot de passe | enregistré dans le coffre (`Save in vault`) |
| TLS | **Disabled** (pas le mode par défaut « Verify CA and host name ») |
| Type | Development |
| Dossier | `team/project` |
| SCRAM only | non coché |
| Tunnel SSH | non |

Observations :

- Un premier remplissage mettait `nine_dev_only` dans **Database**. C’est le mot de passe, pas le nom de la base. Corrigé en `nine` avant le test réussi.
- **Test connection** semble ne rien faire tant qu’on ne fait pas défiler le formulaire. À l’échelle 200 %, le message est en bas de la zone défilante, au-dessus des boutons. Sans type de connexion, le message est « choose a connection type ».
- Tant que le coffre n’existe pas, le champ mot de passe en mode **Save in vault** affiche **Not saved**. Le mot de passe n’est pas saisi dans le dialogue si **Ask every time** est choisi : il est demandé au moment du test.
- Après choix du type **Development**, le test a réussi : `Connected to 18.7 (Debian 18.7-1.pgdg14+1)`, avec l’avertissement que TLS est désactivé.
- Un test suivant, pendant la création du coffre, a échoué : `cannot reach 127.0.0.1:15432: Connection refused (os error 111)`. PostgreSQL n’écoutait plus à cet instant.
- Le coffre a été créé depuis ce dialogue. Le mot de passe maître doit faire au moins 12 caractères. NineSQL ne l’enregistre pas. La session s’est ensuite ouverte.

Session ouverte :

- Navigateur : `postgresql-test`, état connected, badge **2 security warnings**.
- Bulle : « TLS is disabled on this session. » et « Credentials were sent over the network without TLS. »
- Barre d’état : type **Development**, les deux mêmes avertissements.
- Mode **Auto-commit**. Commit et Roll back sont affichés.

Ces avertissements correspondent au TLS laissé sur Disabled. Sur `127.0.0.1` ils sont attendus. Le mode vérifié, avec le certificat de test du conteneur, n’a pas été essayé.

## Éditeur SQL

Dans **Script 1**, relié à `postgresql-test` :

```sql
SELECT current_user, current_database();
```

Exécution avec `Ctrl+Entrée`. Résultat : une ligne, `nine` et `nine`, 11,5 Kio sur un budget de 256,0 Mio, « all rows ».

## Navigateur

- Le premier déploiement est tombé sur `information_schema` (catalogue du serveur), pas sur les données de test.
- Le schéma **public** contient les six tables du jeu « nine » : `all_types`, `big_events`, `customers`, `nine_meta`, `orders`, `products`.
- L’éditeur d’objet de `customers` montre les colonnes `id` (integer, clé primaire), `name`, `email`, `country` (character(2)), `created_at` (timestamp with time zone). Onglets Properties, Columns, Keys / Constraints, Indexes, DDL.
- Le clic droit sur la table propose Open object editor, Generate SQL (SELECT, INSERT, UPDATE, DELETE), Show DDL, Copy DDL, Refresh, les filtres du navigateur et Find object. **Open table data n’est pas dans ce menu.** C’est le comportement décrit comme pas encore construit : l’action passe par `Alt+G` puis `O`, ou par la palette.

## Grille `public.customers`

`Alt+G` puis `O` ouvre un onglet `public.customers`.

- Requête affichée, sans point-virgule : `SELECT * FROM "public"."customers"`.
- Barre : `Table: public.customers`, **Editable (key: id)**.
- 200 lignes chargées, « more rows available ». Le jeu de données en contient 1 000 (`generate_series(1, 1000)` dans `dev/seed/postgresql/01-nine.sql`). Fetch next page et Fetch all sont proposés.
- Le `SELECT` tapé à la main dans Script 1 reste en lecture seule, avec le texte « Read-only: open the table from the navigator (Open table data) to edit its rows. »

Édition :

- La cellule `name` de la ligne `id = 1` a été changée en `client ada`.
- La barre est passée à **1 unsaved changes**, avec **Save** et **Discard**.
- **Save** n’a pas abouti. Le message affiché était : « A run is already going on in this editor: cancel it first. » Le résultat paginé était encore ouvert (bouton **Cancel** en rouge, « more rows available »).
- Après **Cancel**, l’éditeur montrait `SELECT * FROM "public"."customers";` plus une ligne vide. La barre est repassée en lecture seule (« open the table from the navigator »). Le statut est devenu « 200 rows · all rows ». **Save** et la modification en attente ont disparu. `client ada` n’était plus visible : la valeur n’a pas été écrite dans PostgreSQL.

Annuler la lecture pour pouvoir enregistrer a donc fait perdre l’édition, parce que l’onglet n’était plus le résultat de « Open table data ». Il restait à rouvrir la table avec `Alt+G` puis `O`, sans modifier le SQL ni l’exécuter avec `Ctrl+Entrée`, puis à enregistrer tant que la barre dit **Editable (key: id)**. Ce second essai n’a pas été fait.

## Pas encore fait

Passe prévue, pas commencée dans cette session :

- Thème sombre, puis la même passe sous Xorg.
- Types Test et Production, cadre de production.
- Scripts : `Ctrl+N`, `Ctrl+O`, `Ctrl+S`, pastille de modification, fermeture et restauration de session.
- Insertion, suppression, confirmation d’enregistrement (fenêtre étroite, chinois, emojis, drapeaux).
- Filtres, tri, visionneuses, aperçu d’image.
- Export et import CSV sur une connexion Production (lignes « Commits » et « Note »).
- Import DBeaver, puis export vers DBeaver et contrôle dans un workspace jetable.
- Scénario I1 : reconnexion sans retaper le mot de passe, puis `grep` du mot de passe dans les répertoires NineSQL.
- Orca.
- Lancement depuis le menu des applications après installation du `.deb`.

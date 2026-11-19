# Keyboard shortcuts

Every NineSQL action has a default key binding and, except the palette itself, an entry in the command palette (`Ctrl+Shift+P` or `F1`). The tables below are generated from the action registries in the code (`crates/ninesql-ui/src/actions.rs`, `crates/ninesql-editor/src/actions.rs`, `crates/ninesql-grid/src/actions.rs`), which the keymap and the palette also read; the labels are the palette labels.

`Ctrl+K` then `Ctrl+O` is a sequence: press `Ctrl+K`, release, then press `Ctrl+O`. The defaults are the same on every platform (macOS included: `Ctrl`, not `Cmd`).

Where a binding works, from the name in the last column:
- `ninesql::…` (the shell's actions): anywhere in the window, the SQL editor and the grid included;
- `ninesql_editor::…`: while the SQL editor has the keyboard focus;
- `ninesql_grid::…`: while the results grid has the focus.

Text editing inside the SQL editor (moving the cursor, undo, `Ctrl+Alt+Up` / `Ctrl+Alt+Down` to add a cursor above or below) comes from the text input component: those keys are not NineSQL actions, are not in the palette and cannot be changed in `settings.toml`. The navigator tree also has its own keys (arrows, `Enter`, `Home` / `End`, `Page Up` / `Page Down`), described in [Navigator](navigator.md#the-tree).

## General
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| Show command palette | `Ctrl+Shift+P` or `F1` | `ninesql::CommandPalette` |
| Show settings problems | `Ctrl+K` then `Ctrl+E` | `ninesql::ShowSettingsProblems` |

## Windows
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| New window | `Ctrl+Shift+N` | `ninesql::NewWindow` |
| Close window | `Ctrl+Shift+W` | `ninesql::CloseWindow` |
| Quit NineSQL | `Ctrl+Q` | `ninesql::Quit` |

## Tabs and panes
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| New SQL editor | `Ctrl+T` | `ninesql::NewEditorTab` |
| Close editor tab | `Ctrl+W` | `ninesql::CloseTab` |
| Next tab | `Ctrl+Tab` or `Ctrl+PageDown` | `ninesql::NextTab` |
| Previous tab | `Ctrl+Shift+Tab` or `Ctrl+PageUp` | `ninesql::PreviousTab` |
| Split editor right | `Ctrl+\` | `ninesql::SplitRight` |
| Split editor down | `Ctrl+K` then `Ctrl+\` | `ninesql::SplitDown` |
| Focus next pane | `Ctrl+K` then `Ctrl+Right` | `ninesql::FocusNextPane` |
| Focus previous pane | `Ctrl+K` then `Ctrl+Left` | `ninesql::FocusPreviousPane` |

## View
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| Toggle database navigator | `Ctrl+B` | `ninesql::ToggleNavigator` |
| Focus database navigator | `Ctrl+K` then `Ctrl+N` | `ninesql::FocusNavigator` |
| Toggle light / dark theme | `Ctrl+K` then `Ctrl+T` | `ninesql::ToggleTheme` |
| Increase font size | `Ctrl+=` or `Ctrl++` | `ninesql::IncreaseFontSize` |
| Decrease font size | `Ctrl+-` | `ninesql::DecreaseFontSize` |
| Reset font size | `Ctrl+0` | `ninesql::ResetFontSize` |

## Connections and secrets
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| New connection | `Ctrl+Alt+N` | `ninesql::NewConnection` |
| Edit connection | `F4` | `ninesql::EditConnection` |
| Duplicate connection | `Ctrl+K` then `Ctrl+D` | `ninesql::DuplicateConnection` |
| Delete connection | `Ctrl+K` then `Delete` | `ninesql::DeleteConnection` |
| Connect | `Ctrl+K` then `Ctrl+O` | `ninesql::ConnectConnection` |
| Disconnect | `Ctrl+K` then `Ctrl+X` | `ninesql::DisconnectConnection` |
| Test connection | `Ctrl+Alt+T` | `ninesql::TestConnection` |
| Show connection problems | `Ctrl+K` then `Ctrl+L` | `ninesql::ShowConnectionProblems` |
| Lock secrets now | `Ctrl+Alt+L` | `ninesql::LockSecrets` |
| Import from DBeaver | `Ctrl+K` then `Ctrl+I` | `ninesql::ImportFromDbeaver` |
| Import from DBeaver: choose folder | `Ctrl+K` then `Ctrl+Shift+I` | `ninesql::ImportFromDbeaverFolder` |
| Enable connection | `Ctrl+K` then `Ctrl+Shift+E` | `ninesql::EnableConnection` |
| Export connections to DBeaver… | `Ctrl+K` then `Ctrl+Shift+D` | `ninesql::ExportToDbeaver` |

## SQL editor and scripts
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| Choose editor connection | `Ctrl+K` then `Ctrl+P` | `ninesql::ChooseEditorConnection` |
| Commit transaction | `Ctrl+K` then `Ctrl+C` | `ninesql::CommitTransaction` |
| Roll back transaction | `Ctrl+K` then `Ctrl+R` | `ninesql::RollbackTransaction` |
| Format SQL | `Ctrl+Shift+F` | `ninesql::FormatSql` |
| Insert snippet | `Ctrl+K` then `Ctrl+J` | `ninesql::InsertSnippet` |
| New script | `Ctrl+N` | `ninesql::NewScript` |
| Open script… | `Ctrl+O` | `ninesql::OpenScript` |
| Save script | `Ctrl+S` | `ninesql::SaveScript` |
| Save script as… | `Ctrl+Shift+S` | `ninesql::SaveScriptAs` |
| Connect script connection | `Ctrl+K` then `Ctrl+Shift+O` | `ninesql::ConnectScriptConnection` |
| Execute statement | `Ctrl+Enter` | `ninesql_editor::ExecuteStatement` |
| Execute selection | `Ctrl+Shift+Enter` | `ninesql_editor::ExecuteSelection` |
| Execute script | `Alt+X` | `ninesql_editor::ExecuteScript` |
| Cancel execution | `Ctrl+.` | `ninesql_editor::CancelExecution` |
| Show completions | `Ctrl+Space` | `ninesql_editor::TriggerCompletion` |
| Toggle line comment | `Ctrl+/` | `ninesql_editor::ToggleComment` |
| Find | `Ctrl+F` | `ninesql_editor::Find` |
| Find and replace | `Ctrl+H` | `ninesql_editor::FindReplace` |
| Add next occurrence to selection | `Ctrl+D` | `ninesql_editor::SelectNextOccurrence` |
| Go to error | `F8` | `ninesql_editor::GoToError` |
| Select bidirectional control characters | `Alt+F8` | `ninesql_editor::SelectBidiControls` |

## Navigator and objects
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| Refresh navigator node | `F5` | `ninesql::RefreshNavigatorNode` |
| Load more objects | `Ctrl+K` then `Ctrl+M` | `ninesql::LoadMoreObjects` |
| Configure navigator filters | `Ctrl+K` then `Ctrl+F` | `ninesql::ConfigureNavigatorFilters` |
| Find object | `Ctrl+Shift+O` | `ninesql::FindObject` |
| Open object editor | `Ctrl+K` then `Enter` | `ninesql::OpenObjectEditor` |
| Generate SQL: SELECT | `Alt+G` then `S` | `ninesql::GenerateSelectSql` |
| Generate SQL: INSERT | `Alt+G` then `I` | `ninesql::GenerateInsertSql` |
| Generate SQL: UPDATE | `Alt+G` then `U` | `ninesql::GenerateUpdateSql` |
| Generate SQL: DELETE | `Alt+G` then `D` | `ninesql::GenerateDeleteSql` |
| Show DDL | `Alt+G` then `L` | `ninesql::ShowObjectDdl` |
| Copy DDL | `Ctrl+K` then `Ctrl+Shift+C` | `ninesql::CopyObjectDdl` |

## Table data, export and import
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| Export results… | `Ctrl+Alt+E` | `ninesql::ExportResults` |
| Export table… | `Alt+G` then `X` | `ninesql::ExportTable` |
| Import CSV… | `Alt+G` then `M` | `ninesql::ImportCsv` |
| Open table data | `Alt+G` then `O` | `ninesql::OpenTableData` |
| Filter table rows… | `Alt+T` then `F` | `ninesql::FilterTableRows` |
| Filter table by selected value | `Alt+T` then `V` | `ninesql::FilterTableBySelectedValue` |
| Sort table by column ascending | `Alt+T` then `A` | `ninesql::SortTableAscending` |
| Sort table by column descending | `Alt+T` then `D` | `ninesql::SortTableDescending` |
| Clear table filter and sort | `Alt+T` then `C` | `ninesql::ClearTableFilter` |
| Refresh table data | `Alt+T` then `R` | `ninesql::RefreshTableData` |
| Open referenced rows | `Alt+T` then `K` | `ninesql::OpenReferencedRows` |
| Open referencing rows | `Alt+T` then `B` | `ninesql::OpenReferencingRows` |

## Data editing
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| Edit cell… | `Alt+E` then `E` | `ninesql::EditCell` |
| Set cell to NULL | `Alt+E` then `N` | `ninesql::SetCellNull` |
| Insert row… | `Alt+E` then `I` | `ninesql::InsertRow` |
| Duplicate row | `Alt+E` then `C` | `ninesql::DuplicateRow` |
| Delete rows | `Alt+E` then `D` | `ninesql::DeleteRows` |
| Revert row changes | `Alt+E` then `R` | `ninesql::RevertRowChanges` |
| Discard changes | `Alt+E` then `X` | `ninesql::DiscardChanges` |
| Preview changes… | `Alt+E` then `P` | `ninesql::PreviewChanges` |
| Save changes | `Alt+E` then `S` | `ninesql::SaveChanges` |

## Results grid
| Action (palette label) | Default keys | Name for `[keybindings]` |
|---|---|---|
| Copy selected cells | `Ctrl+C` | `ninesql_grid::CopySelection` |
| Copy selected cells with column names | `Ctrl+Shift+C` | `ninesql_grid::CopySelectionWithHeaders` |
| Copy selected cells as CSV | `Ctrl+Alt+C` | `ninesql_grid::CopySelectionAsCsv` |
| Select all loaded rows | `Ctrl+A` | `ninesql_grid::SelectAllCells` |
| Extend selection up | `Shift+Up` | `ninesql_grid::ExtendSelectionUp` |
| Extend selection down | `Shift+Down` | `ninesql_grid::ExtendSelectionDown` |
| Extend selection left | `Shift+Left` | `ninesql_grid::ExtendSelectionLeft` |
| Extend selection right | `Shift+Right` | `ninesql_grid::ExtendSelectionRight` |
| Fetch next page of rows | `Alt+PageDown` | `ninesql_grid::FetchNextPage` |
| Fetch all rows (up to the memory budget) | `Alt+End` | `ninesql_grid::FetchAllRows` |
| Fit column width to content | `Alt+R` | `ninesql_grid::AutoSizeColumn` |
| Fit all column widths to content | `Alt+Shift+R` | `ninesql_grid::AutoSizeAllColumns` |
| Next result tab | `Alt+Right` | `ninesql_grid::NextResultTab` |
| Previous result tab | `Alt+Left` | `ninesql_grid::PreviousResultTab` |
| Show results as grid | `Alt+1` | `ninesql_grid::ShowGridView` |
| Show row as record | `Alt+2` | `ninesql_grid::ShowRecordView` |
| Show results as text | `Alt+3` | `ninesql_grid::ShowTextView` |
| Next record | `Alt+Down` | `ninesql_grid::NextRecord` |
| Previous record | `Alt+Up` | `ninesql_grid::PreviousRecord` |
| Toggle value panel | `Alt+V` | `ninesql_grid::ToggleValuePanel` |
| Next value viewer | `Alt+Shift+V` | `ninesql_grid::NextValueViewer` |
| Toggle aggregate panel | `Alt+A` | `ninesql_grid::ToggleAggregatePanel` |
| Toggle metadata panel | `Alt+M` | `ninesql_grid::ToggleMetadataPanel` |
| Sort loaded rows ascending | `Alt+S` then `A` | `ninesql_grid::SortAscending` |
| Sort loaded rows descending | `Alt+S` then `D` | `ninesql_grid::SortDescending` |
| Filter loaded rows | `Alt+S` then `F` | `ninesql_grid::FilterLoadedRows` |
| Filter loaded rows by selected value | `Alt+S` then `V` | `ninesql_grid::FilterBySelectedValue` |
| Clear sort and filters | `Alt+S` then `C` | `ninesql_grid::ClearSortAndFilter` |
| Copy selected cells as Markdown | `Ctrl+Alt+M` | `ninesql_grid::CopySelectionAsMarkdown` |
| Copy selected cells as JSON | `Ctrl+Alt+J` | `ninesql_grid::CopySelectionAsJson` |
| Copy selected rows as SQL INSERT | `Ctrl+Alt+I` | `ninesql_grid::CopySelectionAsSqlInsert` |

## Change a binding
Bindings are overridden in `~/.config/ninesql/settings.toml`, under `[keybindings]`, with the name from the last column. An override replaces every default binding of its action; an empty list unbinds it. The shell's actions (`ninesql::…`) also accept their short name.
```toml
[keybindings]
"ninesql::NewEditorTab" = "ctrl-n"                        # replaces Ctrl+T
SplitDown = ["ctrl-k ctrl-down", "alt-shift-2"]           # short name, two bindings
ToggleNavigator = []                                      # unbinds
"ninesql_editor::ExecuteStatement" = "f9"                 # editor actions: full name
"ninesql_grid::FetchAllRows" = ["alt-end", "ctrl-shift-end"]
```
Keys are written in lower case with `-` between modifiers and the key (`ctrl`, `alt`, `shift`, `super`), and a space between the keys of a sequence. An unknown action or an invalid binding never stops NineSQL: the defaults of that action are kept and "Show settings problems" (`Ctrl+K` then `Ctrl+E`) lists the problem. If you rebind a key that another action uses by default, check that both still do what you expect.

Known issue: the navigator's "Not connected" hint always reads "Ctrl+K Ctrl+O", even when "Connect" is rebound.

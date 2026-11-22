//! Interface strings. English and Russian live side by side in [`STRINGS`]; every other
//! language is a table of its own in `i18n/<id>.rs`, with the same keys. A key missing in a
//! language falls back to English, so a new string never shows up empty.

use std::sync::atomic::{AtomicU8, Ordering};

mod de;
mod es;
mod fr;
mod ja;
mod pt;
mod zh;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Ru,
    Zh,
    Es,
    Pt,
    De,
    Fr,
    Ja,
}

impl Lang {
    pub const ALL: [Lang; 8] = [Lang::En, Lang::Zh, Lang::Es, Lang::Pt, Lang::De, Lang::Fr, Lang::Ja, Lang::Ru];

    pub fn id(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::Ru => "ru",
            Lang::Zh => "zh",
            Lang::Es => "es",
            Lang::Pt => "pt",
            Lang::De => "de",
            Lang::Fr => "fr",
            Lang::Ja => "ja",
        }
    }

    /// The name of the language in itself.
    pub fn label(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Русский",
            Lang::Zh => "简体中文",
            Lang::Es => "Español",
            Lang::Pt => "Português (Brasil)",
            Lang::De => "Deutsch",
            Lang::Fr => "Français",
            Lang::Ja => "日本語",
        }
    }

    /// The name of the language in English, under its own name.
    pub fn english_name(self) -> &'static str {
        match self {
            Lang::En => "English",
            Lang::Ru => "Russian",
            Lang::Zh => "Chinese, Simplified",
            Lang::Es => "Spanish",
            Lang::Pt => "Portuguese",
            Lang::De => "German",
            Lang::Fr => "French",
            Lang::Ja => "Japanese",
        }
    }

    /// The language of a locale or setting such as `de`, `pt-BR` or `zh_CN`. Chinese is
    /// Simplified only: a Traditional locale (`zh-TW`, `zh-HK`, `zh-Hant`) is not ours.
    pub fn from_id(id: &str) -> Option<Lang> {
        let id = id.to_ascii_lowercase().replace('_', "-");
        let primary = id.split('-').next().unwrap_or_default();
        if primary == "zh" && ["-tw", "-hk", "-mo", "-hant"].iter().any(|traditional| id.contains(traditional)) {
            return None;
        }
        Self::ALL.into_iter().find(|lang| lang.id() == primary)
    }

    /// The language of the system, English when it is not one we have.
    pub fn system() -> Lang {
        sys_locale::get_locale().as_deref().and_then(Lang::from_id).unwrap_or(Lang::En)
    }

    /// The table of a language other than English and Russian.
    fn table(self) -> Option<&'static [(&'static str, &'static str)]> {
        match self {
            Lang::En | Lang::Ru => None,
            Lang::Zh => Some(zh::STRINGS),
            Lang::Es => Some(es::STRINGS),
            Lang::Pt => Some(pt::STRINGS),
            Lang::De => Some(de::STRINGS),
            Lang::Fr => Some(fr::STRINGS),
            Lang::Ja => Some(ja::STRINGS),
        }
    }
}

static CURRENT: AtomicU8 = AtomicU8::new(0);

pub fn set_lang(lang: Lang) {
    let index = Lang::ALL.iter().position(|known| *known == lang).unwrap_or(0);
    CURRENT.store(index as u8, Ordering::Relaxed);
}

pub fn lang() -> Lang {
    Lang::ALL.get(CURRENT.load(Ordering::Relaxed) as usize).copied().unwrap_or(Lang::En)
}

/// `(key, English, Russian)`.
const STRINGS: &[(&str, &str, &str)] = &[
    ("search", "Search sessions", "Поиск по сессиям"),
    ("sessions", "Sessions", "Сессии"),
    ("new_folder", "New folder", "Новая папка"),
    ("new_session", "New session", "Новая сессия"),
    ("refresh", "Rescan history", "Обновить историю"),
    ("sort", "Sort", "Сортировка"),
    ("sort.manual", "Manual", "Вручную"),
    ("sort.updated", "Last activity", "По активности"),
    ("sort.created", "Created", "По созданию"),
    ("sort.title", "Title", "По названию"),
    ("group.none", "No grouping", "Без групп"),
    ("group.pinned", "Pinned", "Закреплённые"),
    ("tab.session", "Session", "Сессия"),
    ("tab.files", "Files", "Файлы"),
    ("tab.changes", "Changes", "Изменения"),
    ("memory.title", "Bowyer memory", "Память Bowyer"),
    ("memory.connected", "connected", "подключена"),
    ("memory.this_project", "For this project", "Для этого проекта"),
    ("memory.not_connected", "not connected", "не подключена"),
    ("memory.about", "An MCP server: the harness searches past sessions of this project and keeps notes, instead of carrying them in its context. Off here, every call is refused.", "MCP-сервер: харнес ищет прошлые сессии проекта и хранит заметки, а не держит их в контексте. Выключено здесь — вызовы отклоняются."),
    ("memory.run", "Run in a terminal", "Запустить в терминале"),
    ("memory.notes", "Notes of this project", "Заметки проекта"),
    ("code.copy", "Copy", "Копировать"),
    ("code.copied", "Copied", "Скопировано"),
    ("code.run", "Run", "Запустить"),
    ("code.started", "Started in a terminal", "Запущено в терминале"),
    ("status.working", "working", "работает"),
    ("status.waiting", "waiting", "ждёт"),
    ("status.your_turn", "your turn", "твой ход"),
    ("changes.no_repo", "The folder is not in a git repository", "Папка не в git-репозитории"),
    ("changes.clean", "No changes: the working tree is clean", "Изменений нет"),
    ("changes.staged", "Staged", "В индексе"),
    ("changes.history", "History", "История"),
    ("changes.push", "Push", "Push"),
    ("changes.pushing", "Pushing…", "Отправка…"),
    ("changes.pushed", "Pushed", "Отправлено"),
    ("changes.fetch", "Fetch", "Fetch"),
    ("changes.fetching", "Fetching…", "Получение…"),
    ("changes.fetched", "Fetched", "Получено"),
    ("changes.pull", "Pull", "Pull"),
    ("changes.pulling", "Pulling…", "Подтягивание…"),
    ("changes.pulled", "Pulled", "Подтянуто"),
    ("changes.branches", "Local branches: click one to switch", "Локальные ветки: клик переключает"),
    ("changes.switched", "Switched to", "Переключено на"),
    ("changes.detached", "detached HEAD", "отделённый HEAD"),
    ("changes.message", "Commit message (Enter commits, Shift+Enter for a new line)", "Сообщение коммита (Enter — коммит, Shift+Enter — новая строка)"),
    ("changes.commit", "Commit", "Закоммитить"),
    ("changes.committing", "Committing…", "Коммит…"),
    ("changes.committed", "Committed", "Закоммичено"),
    ("changes.unstaged", "Changes", "Изменения"),
    ("changes.stage", "Stage", "В индекс"),
    ("changes.unstage", "Unstage", "Из индекса"),
    ("changes.stage_file", "Stage file", "Файл в индекс"),
    ("changes.unstage_file", "Unstage file", "Файл из индекса"),
    ("changes.discard", "Discard", "Откатить"),
    ("changes.confirm_discard", "Discard for good?", "Откатить насовсем?"),
    ("changes.no_diff", "No differences on this side", "Различий нет"),
    ("changes.more_lines", "more lines are not shown", "строк не показано"),
    ("files.empty", "The folder is empty", "Папка пуста"),
    ("files.back", "Back to the chat", "К разговору"),
    ("files.binary", "A binary file", "Двоичный файл"),
    ("files.bad_picture", "The picture cannot be read", "Картинку не прочитать"),
    ("files.truncated", "Only the first 2 MB are shown", "Показаны первые 2 МБ"),
    ("group.project", "By project", "По проектам"),
    ("group.environment", "By environment", "По окружениям"),
    ("group.harness", "By harness", "По харнесам"),
    ("scanning", "Reading history…", "Читаю историю…"),
    ("no_session", "Pick a session on the left, or start a new one.", "Выберите сессию слева или начните новую."),
    ("loading", "Loading the session…", "Загружаю сессию…"),
    ("you", "You", "Вы"),
    ("thinking", "Thinking", "Размышления"),
    ("compacted", "Context compacted. Summary:", "Контекст сжат. Итог:"),
    ("resume", "Resume in terminal", "Продолжить в терминале"),
    ("open_terminal", "Open a terminal here", "Открыть терминал здесь"),
    ("compact", "Compact", "Сжать"),
    ("compact.hint", "Runs /compact as a turn here: the summary replaces the old context. Costs about what the right panel says.", "Запускает /compact ходом здесь: итог заменяет старый контекст. Стоит примерно столько, сколько показано справа."),
    ("info", "Session", "Сессия"),
    ("environment", "Environment", "Окружение"),
    ("folder", "Folder", "Папка"),
    ("branch", "Branch", "Ветка"),
    ("model", "Model", "Модель"),
    ("context", "Context", "Контекст"),
    ("context.exact", "Exact", "Точно"),
    ("context.exact_busy", "Counting…", "Считаю…"),
    ("context.exact_tip", "Ask Claude Code how it counts this context (its /context; no model call)", "Спросить Claude Code, как он сам считает этот контекст (его /context, без вызова модели)"),
    ("context.exact_failed", "Claude Code did not count the context", "Claude Code не посчитал контекст"),
    ("context.by_claude", "As Claude Code counts it; free space and deferred tools left out", "Как считает сам Claude Code; свободное место и отложенные тулы не показаны"),
    ("context.estimated", "estimated", "оценка"),
    ("of", "of", "из"),
    ("outputs", "Changed files", "Изменённые файлы"),
    ("no_outputs", "No files changed", "Файлы не менялись"),
    ("tasks", "Commands and agents", "Команды и агенты"),
    ("no_tasks", "Nothing ran", "Ничего не запускалось"),
    ("tokens", "Tokens", "Токены"),
    ("cost", "Cost", "Стоимость"),
    ("theme", "Theme", "Тема"),
    ("search.results", "results", "найдено"),
    ("search.rerank", "Rank", "Упорядочить"),
    ("search.rerank_tip", "A model orders the first 30 results by what the query means: Haiku with one small call on your Claude account, or a local model of Ollama or LM Studio for free.", "Модель упорядочит первые 30 результатов по смыслу запроса: Haiku — один небольшой вызов на вашем аккаунте Claude, локальная модель Ollama или LM Studio — бесплатно."),
    ("search.ranker_tip", "Choose the model that ranks", "Выбрать модель, которая упорядочивает"),
    ("search.ranker_account", "Claude account", "аккаунт Claude"),
    ("search.ranker_looking", "Looking for local models…", "Ищу локальные модели…"),
    ("search.ranker_none", "No local models: Ollama and LM Studio are not running", "Локальных моделей нет: Ollama и LM Studio не запущены"),
    ("search.ranking", "Ranking…", "Упорядочиваю…"),
    ("search.ranked", "Ordered by", "Упорядочено:"),
    ("search.rank_failed", "The model did not answer", "Модель не ответила"),
    ("selection.ask", "Ask about this", "Спросить об этом"),
    ("menu.rename", "Rename", "Переименовать"),
    ("menu.new_subfolder", "New folder inside", "Новая папка внутри"),
    ("menu.delete_folder", "Delete folder", "Удалить папку"),
    ("menu.confirm_delete", "Click again: delete with its subfolders", "Ещё раз: удалить вместе с подпапками"),
    ("menu.folder_deleted", "Folder deleted, its sessions moved to the top", "Папка удалена, сессии перенесены наверх"),
    ("menu.pin", "Pin", "Закрепить"),
    ("menu.unpin", "Unpin", "Открепить"),
    ("menu.reset_name", "Restore the original name", "Вернуть исходное имя"),
    ("menu.move_out", "Move out of the folder", "Вынуть из папки"),
    ("menu.copy_id", "Copy session ID", "Скопировать ID сессии"),
    ("theme.import", "Import a VS Code theme (.json) or an .icls scheme", "Импорт темы VS Code (.json) или схемы .icls"),
    ("theme.imported", "Theme imported", "Тема импортирована"),
    ("theme.import_failed", "Theme not imported", "Тема не импортирована"),
    ("theme.edit", "Edit the theme", "Изменить тему"),
    ("notify.waiting", "Waiting for you", "Ждёт тебя"),
    ("notify.your_turn", "Your turn", "Твой ход"),
    ("notify.asks", "Asks for your answer", "Ждёт ответа на вопрос"),
    ("notify.done", "The turn is done", "Ход закончен"),
    ("notify.failed", "The turn ended with an error", "Ход закончился ошибкой"),
    ("window.minimize", "Minimize", "Свернуть"),
    ("window.maximize", "Maximize", "Развернуть"),
    ("window.restore", "Restore", "Восстановить"),
    ("window.close", "Close", "Закрыть"),
    ("settings", "Settings", "Настройки"),
    ("settings.close", "Close the settings", "Закрыть настройки"),
    ("settings.general", "General", "Общие"),
    ("settings.appearance", "Appearance", "Оформление"),
    ("settings.sessions", "Sessions", "Сессии"),
    ("settings.harnesses", "Harnesses", "Харнесы"),
    ("settings.hosts", "SSH hosts", "SSH-хосты"),
    ("settings.terminal", "Terminal", "Терминал"),
    ("settings.search", "Search", "Поиск"),
    ("settings.memory", "Memory", "Память"),
    ("settings.updates", "Updates", "Обновления"),
    ("settings.about", "About", "О программе"),
    ("settings.language", "Language", "Язык"),
    ("settings.notify", "Notifications", "Уведомления"),
    ("settings.notify_hint", "A system notification when a session you watch waits for you, finishes or fails while the window is in the background", "Системное уведомление, когда отслеживаемая сессия ждёт вас, закончила или упала, пока окно в фоне"),
    ("settings.tray_close", "Keep running in the tray", "Оставаться в трее"),
    ("settings.tray_close_hint", "Closing the window hides it next to the icon in the notification area; Quit in the icon's menu ends Bowyer", "Закрытие окна прячет его к значку в области уведомлений; выйти — через меню значка"),
    ("settings.tray_none", "The icon in the notification area is Windows-only for now", "Значок в области уведомлений пока есть только на Windows"),
    ("settings.data", "Data", "Данные"),
    ("settings.data_folder", "Library, themes and caches", "Библиотека, темы и кэши"),
    ("settings.show", "Show", "Показать"),
    ("settings.themes", "Themes", "Темы"),
    ("settings.sort", "Order", "Порядок"),
    ("settings.grouping", "Groups", "Группы"),
    ("settings.grouping_hint", "Sessions in worktrees of a repository go with the repository", "Сессии в рабочих деревьях репозитория идут вместе с репозиторием"),
    ("settings.apply", "Apply", "Применить"),
    ("settings.program", "Program", "Программа"),
    ("settings.not_found", "`{program}` is not on PATH", "`{program}` не найден в PATH"),
    ("settings.model", "Model of new turns", "Модель новых ходов"),
    ("settings.model_hint", "The same choice as in the field under the conversation", "Тот же выбор, что под полем ввода"),
    ("settings.hosts_hint", "Sessions of these hosts are read over SSH. Forgetting a host takes its sessions out of the tree; nothing on the host changes.", "Сессии этих хостов читаются по SSH. Если забыть хост, его сессии уйдут из дерева; на самом хосте ничего не меняется."),
    ("settings.no_hosts", "No hosts yet", "Хостов пока нет"),
    ("settings.key_default", "The key from ssh's own configuration", "Ключ из настроек самого ssh"),
    ("settings.forget", "Forget", "Забыть"),
    ("settings.add_host", "Add a host", "Добавить хост"),
    ("settings.shell", "Shell of the terminal", "Оболочка терминала"),
    ("settings.shell_hint", "The shell the terminal of the window opens on this computer. In PowerShell 7 on a non-Latin layout Ctrl+C at the prompt types a letter; Windows PowerShell does not.", "Оболочка, которую открывает терминал окна на этом компьютере. В PowerShell 7 на нелатинской раскладке Ctrl+C в строке ввода печатает букву; в Windows PowerShell — нет."),
    ("settings.shell_auto", "Automatic", "Автоматически"),
    ("settings.ranker", "Ordering of search results", "Упорядочивание результатов поиска"),
    ("settings.ranker_hint", "The model that orders the first results by meaning: Haiku through your Claude account, or a model of Ollama or LM Studio on this computer", "Модель, которая упорядочивает первые результаты по смыслу: Haiku через ваш аккаунт Claude или модель Ollama либо LM Studio на этом компьютере"),
    ("settings.memory_on", "Memory for harnesses", "Память для харнесов"),
    ("settings.memory_hint", "The memory server answers harnesses only while this is on", "Сервер памяти отвечает харнесам, только пока это включено"),
    ("settings.mcp", "Connect a harness", "Подключить харнес"),
    ("settings.mcp_hint", "Run the command in a terminal once; Bowyer does not write the configuration of a harness itself", "Выполните команду в терминале один раз; Bowyer сам не пишет конфигурацию харнеса"),
    ("settings.version", "Version", "Версия"),
    ("settings.license", "License", "Лицензия"),
    ("settings.license_hint", "Free software: you may share and change it under the same license", "Свободная программа: её можно распространять и менять на тех же условиях"),
    ("settings.notices", "Licenses of the parts inside", "Лицензии составных частей"),
    ("update.no_feed", "No update feed in this build", "В этой сборке нет адреса обновлений"),
    ("update.checking", "Looking for a new version…", "Ищу новую версию…"),
    ("update.available", "Version {version} is out", "Вышла версия {version}"),
    ("update.available_tip", "A new version is out", "Вышла новая версия"),
    ("update.up_to_date", "This is the newest version", "Это самая новая версия"),
    ("update.installing", "Downloading and checking the update; Bowyer restarts when it is in place", "Скачиваю и проверяю обновление; Bowyer перезапустится, когда оно встанет"),
    ("update.check_now", "Check now", "Проверить сейчас"),
    ("update.install", "Install and restart", "Установить и перезапустить"),
    ("update.open_page", "Open the release page", "Открыть страницу выпуска"),
    ("update.no_key", "This build has no key to check updates with, so it does not install them itself", "У этой сборки нет ключа для проверки обновлений, поэтому сама она их не ставит"),
    ("update.unmanaged", "This copy was not installed by the Bowyer installer or as an AppImage: update it the way you installed it", "Эта копия поставлена не установщиком Bowyer и не как AppImage: обновите её тем же способом, каким ставили"),
    ("update.no_package", "The release has no package for this system", "У выпуска нет пакета для этой системы"),
    ("update.auto", "Check for updates", "Проверять обновления"),
    ("update.auto_hint", "On start and twice a day; nothing is installed without your click", "При запуске и дважды в день; без вашего нажатия ничего не ставится"),
    ("update.feed", "Feed", "Адрес"),
    ("tray.open", "Open Bowyer", "Открыть Bowyer"),
    ("tray.quit", "Quit Bowyer", "Выйти из Bowyer"),
    ("tree.show_more", "Show {n} more", "Показать ещё {n}"),
    ("tree.collapse_all", "Collapse or expand all", "Свернуть или развернуть всё"),
    ("terminal.ended", "The program ended. Ctrl+` opens a new shell.", "Программа завершилась. Ctrl+` откроет новую оболочку."),
    ("terminal.failed", "The terminal did not start", "Терминал не запустился"),
    ("terminal.back", "Conversation", "К разговору"),
    ("terminal.close", "Close the terminal", "Закрыть терминал"),
    ("terminal.ssh_failed", "ssh could not connect (see above).", "ssh не смог подключиться (см. выше)."),
    ("terminal.press_enter", "Press Enter to close", "Нажмите Enter, чтобы закрыть"),
    ("theme.editor", "Theme", "Тема"),
    ("theme.copy", "copy", "копия"),
    ("theme.based_on", "Based on", "На основе"),
    ("theme.hint", "click a colour to change it", "щёлкните цвет, чтобы изменить"),
    ("theme.reset", "Back to the colour of the parent theme", "Вернуть цвет родительской темы"),
    ("theme.save", "Save", "Сохранить"),
    ("theme.cancel", "Cancel", "Отмена"),
    ("theme.saved", "Theme saved", "Тема сохранена"),
    ("theme.save_failed", "Theme not saved", "Тема не сохранена"),
    ("theme.no_name", "the theme has no name", "у темы нет названия"),
    ("theme.group.window", "Window", "Окно"),
    ("theme.group.code", "Code", "Код"),
    ("theme.group.editor", "Editor", "Редактор"),
    ("theme.group.status", "Status", "Состояния"),
    ("local", "This computer", "Этот компьютер"),
    ("tools.show", "Show tool calls", "Показать вызовы"),
    ("tools.hide", "Hide tool calls", "Скрыть вызовы"),
    ("expand", "Expand", "Развернуть"),
    ("collapse", "Collapse", "Свернуть"),
    ("files", "Files", "Файлы"),
    ("rename", "Rename", "Переименовать"),
    ("pin", "Pin", "Закрепить"),
    ("unpin", "Unpin", "Открепить"),
    ("delete_folder", "Delete folder", "Удалить папку"),
    ("drop_here", "Drop to move here", "Отпустите, чтобы переместить"),
    ("results", "Results", "Результаты"),
    ("no_results", "Nothing found", "Ничего не найдено"),
    ("mcp", "MCP servers and plugins", "MCP-серверы и плагины"),
    ("error", "Error", "Ошибка"),
    ("sessions_count", "sessions", "сессий"),
    ("live.send", "Send", "Отправить"),
    ("live.stop", "Stop", "Стоп"),
    ("live.stop_command", "stop", "стоп"),
    ("live.stopping_command", "stopping…", "останавливаю…"),
    ("live.stop_command_failed", "Could not stop the command", "Не удалось остановить команду"),
    ("live.running", "Running", "Выполняется"),
    ("live.done", "Turn finished", "Ход завершён"),
    ("live.stopped", "Stopping the turn…", "Останавливаю ход…"),
    ("live.unsupported", "This harness runs in the terminal for now", "Этот харнес пока работает в терминале"),
    ("ssh.add", "Add an SSH host", "Добавить SSH-хост"),
    ("ssh.remove", "Remove", "Убрать"),
    ("ssh.removed", "removed; its sessions left the tree", "убран, его сессии ушли из дерева"),
    ("cmd.new", "New session", "Новая сессия"),
    ("cmd.model", "Switch the model", "Сменить модель"),
    ("cmd.permission", "Switch permissions", "Сменить разрешения"),
    ("cmd.attach", "Attach files", "Прикрепить файлы"),
    ("cmd.stop", "Stop the running turn", "Остановить ход"),
    ("cmd.terminal", "Terminal in the session folder", "Терминал в папке сессии"),
    ("cmd.resume", "Resume in a terminal", "Продолжить в терминале"),
    ("cmd.compact", "Compact the context in a terminal", "Сжать контекст в терминале"),
    ("cmd.user_command", "user command", "команда пользователя"),
    ("cmd.project_command", "project command", "команда проекта"),
    ("cmd.user_skill", "user skill", "скилл пользователя"),
    ("cmd.project_skill", "project skill", "скилл проекта"),
    ("composer.attach", "Attach files: or drop them here, or paste a picture", "Прикрепить файлы: можно перетащить или вставить картинку"),
    ("composer.indexing", "Listing the files…", "Читаю список файлов…"),
    ("live.uploading", "Copying the files to the host…", "Копирую файлы на хост…"),
    ("ssh.key", "key", "ключ"),
    ("ssh.key_default", "key: from ssh config", "ключ: из ssh config"),
    ("ssh.connect", "Connect", "Подключить"),
    ("ssh.probing", "Connecting…", "Подключаюсь…"),
    ("ssh.unknown", "this host is not added", "этот хост не добавлен"),
    ("ssh.password", "Password", "Пароль"),
    ("ssh.password_auth", "sign-in: password", "вход: по паролю"),
    ("ssh.password_needed", "Type the password of this host", "Введите пароль этого хоста"),
    ("ssh.password_stored", "The password is kept in Windows Credential Manager, never in Bowyer's files.", "Пароль хранится в диспетчере учётных данных Windows, а не в файлах Bowyer."),
    ("ssh.unknown_key", "Bowyer does not know this host's key yet. Compare the fingerprint with the one the host shows before you trust it:", "Bowyer ещё не знает ключ этого хоста. Сверьте отпечаток с тем, что показывает сам хост, прежде чем доверять ему:"),
    ("ssh.trust", "Trust and connect", "Доверять и подключить"),
    ("ssh.keyscan_failed", "Could not read the host key", "Не удалось прочитать ключ хоста"),
    ("live.no_folder", "The session has no working folder", "У сессии нет рабочей папки"),
    ("model.default", "default model", "модель по умолчанию"),
    ("composer.placeholder", "Message · / commands · @ files · Enter to send, Shift+Enter for a new line", "Сообщение · / команды · @ файлы · Enter — отправить, Shift+Enter — новая строка"),
    ("permission.settings", "permissions: settings", "права: из настроек"),
    ("permission.accept_edits", "permissions: accept edits", "права: принимать правки"),
    ("permission.bypass", "permissions: all", "права: всё разрешено"),
    ("ask.execute", "wants to run a command", "хочет запустить команду"),
    ("ask.edit", "wants to change a file", "хочет изменить файл"),
    ("ask.fetch", "wants to fetch from the web", "хочет загрузить из сети"),
    ("ask.other", "wants to use a tool", "хочет использовать инструмент"),
    ("ask.question", "asks you", "спрашивает"),
    ("ask.dismiss", "Don't answer", "Не отвечать"),
    ("ask.send", "Send answers", "Отправить ответы"),
    ("ask.send_one", "Send answer", "Отправить ответ"),
    ("ask.own", "Your own answer…", "Свой ответ…"),
    ("ask.allow_once", "Allow once", "Разрешить один раз"),
    ("ask.allow_always", "Always allow", "Разрешать всегда"),
    ("ask.reject_once", "Refuse", "Отказать"),
    ("ask.reject_always", "Always refuse", "Отказывать всегда"),
    ("cost.session", "Session cost", "Цена сессии"),
    ("cost.reported", "harness report (its last run)", "отчёт харнеса (последний запуск)"),
    ("cost.computed", "API prices", "по ценам API"),
    ("cost.subscription", "covered by subscription", "входит в подписку"),
    ("cost.compact", "Compact now", "Компакт сейчас"),
    ("cost.next_turn", "Next turn input", "Вход следующего хода"),
    ("cache", "Prompt cache", "Кэш промпта"),
    ("cache.warm", "warm", "тёплый"),
    ("cache.cold", "cold", "холодный"),
    ("cache.left", "left", "осталось"),
    ("cache.expired", "expired", "истёк"),
    ("cache.unknown", "no data", "нет данных"),
    ("cache.auto", "auto · idle", "авто · простой"),
    ("price.unknown", "no price for this model", "нет цены для этой модели"),
    ("part.system", "System prompt, tools and the rest", "Системный промпт, тулы и прочее"),
    ("part.memory", "Memory files (CLAUDE.md, AGENTS.md)", "Файлы памяти (CLAUDE.md, AGENTS.md)"),
    ("part.prompts", "Your messages", "Ваши сообщения"),
    ("part.answers", "Answers", "Ответы"),
    ("part.thinking", "Thinking", "Размышления"),
    ("part.tool_calls", "Tool calls", "Вызовы тулов"),
    ("part.tool_results", "Tool results", "Результаты тулов"),
    ("part.images", "Images", "Изображения"),
    ("part.summary", "Compaction summary", "Итог сжатия"),
    ("claude_part.system_prompt", "System prompt", "Системный промпт"),
    ("claude_part.system_tools", "System tools", "Системные тулы"),
    ("claude_part.mcp_tools", "MCP tools", "Тулы MCP"),
    ("claude_part.mcp_instructions", "MCP server instructions", "Инструкции серверов MCP"),
    ("claude_part.agents", "Custom agents", "Свои агенты"),
    ("claude_part.memory", "Memory files", "Файлы памяти"),
    ("claude_part.skills", "Skills", "Навыки"),
    ("claude_part.messages", "Messages", "Сообщения"),
];

/// The text of a key in the current language.
pub fn t(key: &str) -> &'static str {
    let Some((_, en, ru)) = STRINGS.iter().find(|(k, _, _)| *k == key) else { return "?" };
    let lang = lang();
    match lang {
        Lang::En => en,
        Lang::Ru => ru,
        _ => lang.table().and_then(|table| table.iter().find(|(k, _)| *k == key)).map_or(en, |(_, text)| text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn keys_are_unique_and_filled() {
        let mut keys = HashSet::new();
        for (key, en, ru) in STRINGS {
            assert!(keys.insert(*key), "{key} twice");
            assert!(!en.is_empty() && !ru.is_empty(), "{key} empty");
        }
    }

    /// The placeholders of a text, such as `{n}`.
    fn placeholders(text: &str) -> Vec<&str> {
        let mut found: Vec<&str> = text.match_indices('{').filter_map(|(start, _)| text[start..].find('}').map(|end| &text[start..=start + end])).collect();
        found.sort_unstable();
        found
    }

    #[test]
    fn every_language_has_every_key_once_with_its_placeholders() {
        let english: Vec<&str> = STRINGS.iter().map(|(key, _, _)| *key).collect();
        for lang in Lang::ALL {
            let Some(table) = lang.table() else { continue };
            let mut seen = HashSet::new();
            for (key, text) in table {
                assert!(seen.insert(*key), "{}: {key} twice", lang.id());
                assert!(!text.trim().is_empty(), "{}: {key} empty", lang.id());
                let (_, en, _) = STRINGS.iter().find(|(k, _, _)| k == key).unwrap_or_else(|| panic!("{}: {key} is not a key", lang.id()));
                assert_eq!(placeholders(text), placeholders(en), "{}: {key} placeholders", lang.id());
            }
            let missing: Vec<&&str> = english.iter().filter(|key| !seen.contains(**key)).collect();
            assert!(missing.is_empty(), "{}: missing {missing:?}", lang.id());
        }
    }

    #[test]
    fn locales_find_their_language() {
        assert_eq!(Lang::from_id("ru-RU"), Some(Lang::Ru));
        assert_eq!(Lang::from_id("de-DE"), Some(Lang::De));
        assert_eq!(Lang::from_id("pt_BR"), Some(Lang::Pt));
        assert_eq!(Lang::from_id("zh-CN"), Some(Lang::Zh));
        assert_eq!(Lang::from_id("zh-Hans-SG"), Some(Lang::Zh));
        assert_eq!(Lang::from_id("zh-TW"), None);
        assert_eq!(Lang::from_id("ko-KR"), None);
        assert_eq!(Lang::from_id("en"), Some(Lang::En));
        assert_eq!(Lang::from_id("e"), None, "a prefix of an id is not a language");
    }
}

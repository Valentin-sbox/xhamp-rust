use axum::{
    extract::{Path as AxumPath, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    fs::{self, File, OpenOptions},
    io::{self, Read},
    net::{SocketAddr, TcpStream},
    path::{Component, Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
use tokio::time::sleep;
use zip::ZipArchive;

const PANEL_ADDRESS: &str = "127.0.0.1:8765";
const DEFAULT_MYSQL_PORT: u16 = 3307;
const PHP_CATALOG_URL: &str = "https://windows.php.net/downloads/releases/releases.json";
const MARIADB_CATALOG_URL: &str = "https://downloads.mariadb.org/rest-api/mariadb/";
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    data: PathBuf,
    client: Client,
    processes: Arc<Mutex<Processes>>,
    install_lock: Arc<tokio::sync::Mutex<()>>,
}

fn installed_versions(root: &Path, kind: &str) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut versions: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let path = entry.path();
            match kind {
                "php" => path.join("php.exe").is_file(),
                _ => {
                    (path.join("bin").join("mariadbd.exe").is_file()
                        || path.join("bin").join("mysqld.exe").is_file())
                        && (path.join("bin").join("mariadb-install-db.exe").is_file()
                            || path.join("bin").join("mysql_install_db.exe").is_file())
                }
            }
        })
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    versions.sort_by(|left, right| version_cmp(left, right));
    versions
}

#[derive(Default)]
struct Processes {
    mysql: Option<Child>,
    mysql_bin: Option<PathBuf>,
    mysql_port: Option<u16>,
    php: Option<Child>,
}

#[derive(Serialize)]
struct Status {
    data_path: String,
    mysql: String,
    php: String,
    selected_php: String,
    selected_mariadb: String,
    installed_php: Vec<String>,
    installed_mariadb: Vec<String>,
    mysql_port: Option<u16>,
    projects: Vec<String>,
}

#[derive(Serialize, Clone)]
struct Release {
    version: String,
    download_url: String,
    sha256: String,
}

#[derive(Serialize)]
struct Catalog {
    php: Vec<Release>,
    mariadb: Vec<Release>,
}

#[derive(Deserialize)]
struct NewProject {
    name: String,
}

#[derive(Deserialize)]
struct StartPhp {
    project: String,
    port: u16,
}

#[derive(Deserialize)]
struct SelectPhp {
    version: String,
}

#[derive(Deserialize)]
struct SelectMariaDb {
    version: String,
}

#[derive(Deserialize)]
struct StartMariaDb {
    port: u16,
}

#[derive(Deserialize)]
struct InstallRelease {
    kind: String,
    version: String,
}

#[derive(Serialize)]
struct Message {
    message: String,
}

type ApiResult<T> = Result<Json<T>, (StatusCode, String)>;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let data = env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(env::temp_dir)
        .join("XhampRust");
    fs::create_dir_all(data.join("projects"))?;
    fs::create_dir_all(data.join("downloads"))?;
    fs::create_dir_all(data.join("runtimes").join("php"))?;
    fs::create_dir_all(data.join("runtimes").join("mariadb"))?;
    fs::create_dir_all(data.join("logs"))?;

    let client = Client::builder()
        .user_agent(concat!("XhampRust/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15 * 60))
        .build()?;
    let state = AppState {
        data,
        client,
        processes: Arc::new(Mutex::new(Processes::default())),
        install_lock: Arc::new(tokio::sync::Mutex::new(())),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/catalog", get(catalog))
        .route("/api/projects", post(create_project))
        .route("/api/php/start", post(start_php))
        .route("/api/php/stop", post(stop_php))
        .route("/api/php/select", post(select_php))
        .route("/api/mysql/start", post(start_mysql))
        .route("/api/mysql/stop", post(stop_mysql))
        .route("/api/mysql/select", post(select_mariadb))
        .route("/api/install", post(install_release))
        .route("/api/projects/{name}", get(project_details))
        .layer(middleware::from_fn(validate_host))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(PANEL_ADDRESS).await?;
    let panel_url = format!("http://{PANEL_ADDRESS}");
    println!("Xhamp Rust está listo en {panel_url}");
    if let Err(error) = open_browser(&panel_url) {
        eprintln!("No pude abrir el navegador automáticamente: {error}");
    }
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../static/index.html"))
}

async fn validate_host(request: Request, next: Next) -> Response {
    let allowed = request
        .headers()
        .get(axum::http::header::HOST)
        .and_then(|host| host.to_str().ok())
        .map(|host| matches!(host, "127.0.0.1:8765" | "localhost:8765"))
        .unwrap_or(false);
    if !allowed {
        return (
            StatusCode::FORBIDDEN,
            "Solo se permite acceder desde localhost.",
        )
            .into_response();
    }
    next.run(request).await
}

async fn status(State(state): State<AppState>) -> ApiResult<Status> {
    let mysql_managed = process_running(&state.processes, "mysql");
    let php_running = process_running(&state.processes, "php");
    let projects = list_projects(&state.data.join("projects"));
    let installed_php = installed_versions(&state.data.join("runtimes").join("php"), "php");
    let installed_mariadb =
        installed_versions(&state.data.join("runtimes").join("mariadb"), "mariadb");
    let selected_php = selected_version(&state, "php").unwrap_or_else(|| "Ninguna".to_string());
    let selected_mariadb =
        selected_version(&state, "mariadb").unwrap_or_else(|| "Ninguna".to_string());
    let mysql_port = active_mysql_port(&state.processes);

    Ok(Json(Status {
        data_path: state.data.display().to_string(),
        mysql: if mysql_managed {
            "iniciado por Xhamp Rust".to_string()
        } else if port_open(mysql_port.unwrap_or(DEFAULT_MYSQL_PORT)) {
            format!(
                "puerto {} ocupado (servicio externo)",
                mysql_port.unwrap_or(DEFAULT_MYSQL_PORT)
            )
        } else {
            "detenido".to_string()
        },
        php: if php_running {
            "servidor iniciado".to_string()
        } else {
            "detenido".to_string()
        },
        selected_php,
        selected_mariadb,
        installed_php,
        installed_mariadb,
        mysql_port: if mysql_managed { mysql_port } else { None },
        projects,
    }))
}

fn active_mysql_port(processes: &Arc<Mutex<Processes>>) -> Option<u16> {
    processes.lock().ok()?.mysql_port
}

async fn catalog(State(state): State<AppState>) -> ApiResult<Catalog> {
    let php = php_releases(&state.client).await.map_err(api_error)?;
    let mariadb = mariadb_releases(&state.client).await.map_err(api_error)?;
    Ok(Json(Catalog { php, mariadb }))
}

async fn create_project(
    State(state): State<AppState>,
    Json(request): Json<NewProject>,
) -> ApiResult<Message> {
    if !valid_project_name(&request.name) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Usá un nombre con letras, números, guion o guion bajo.".into(),
        ));
    }

    let path = state.data.join("projects").join(&request.name);
    fs::create_dir(&path).map_err(|error| {
        let status = if error.kind() == io::ErrorKind::AlreadyExists {
            StatusCode::CONFLICT
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        };
        (status, format!("No se pudo crear el proyecto: {error}"))
    })?;
    let starter =
        include_str!("../static/project-index.php").replace("__PROJECT_NAME__", &request.name);
    fs::write(path.join("index.php"), starter).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Se creó la carpeta, pero no se pudo escribir index.php: {error}"),
        )
    })?;
    Ok(Json(Message {
        message: format!("Proyecto creado en {}", path.display()),
    }))
}

async fn project_details(
    State(state): State<AppState>,
    AxumPath(name): AxumPath<String>,
) -> ApiResult<Message> {
    if !valid_project_name(&name) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Nombre de proyecto inválido.".into(),
        ));
    }
    let path = state.data.join("projects").join(name);
    if !path.is_dir() {
        return Err((StatusCode::NOT_FOUND, "No existe ese proyecto.".into()));
    }
    Ok(Json(Message {
        message: path.display().to_string(),
    }))
}

async fn start_php(
    State(state): State<AppState>,
    Json(request): Json<StartPhp>,
) -> ApiResult<Message> {
    if !valid_project_name(&request.project) || !(1024..=65535).contains(&request.port) {
        return Err((
            StatusCode::BAD_REQUEST,
            "Proyecto o puerto inválido.".into(),
        ));
    }
    if process_running(&state.processes, "php") {
        return Err((
            StatusCode::CONFLICT,
            "Ya hay un servidor PHP iniciado por el panel; detenelo antes de abrir otro.".into(),
        ));
    }
    let project = state.data.join("projects").join(&request.project);
    if !project.is_dir() {
        return Err((
            StatusCode::NOT_FOUND,
            "Creá el proyecto antes de iniciarlo.".into(),
        ));
    }
    if port_open(request.port) {
        return Err((
            StatusCode::CONFLICT,
            format!("El puerto {} ya está en uso.", request.port),
        ));
    }

    let php = selected_php_path(&state).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "Primero descargá e instalá una versión de PHP desde el catálogo.".into(),
        )
    })?;
    let (stdout, stderr) = log_streams(&state.data, "php-server").map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("No se pudo abrir el log de PHP: {error}"),
        )
    })?;
    let child = Command::new(&php)
        .arg("-c")
        .arg(php.parent().unwrap_or(Path::new(".")).join("php.ini"))
        .arg("-S")
        .arg(format!("127.0.0.1:{}", request.port))
        .arg("-t")
        .arg(&project)
        .current_dir(&project)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("No se pudo iniciar PHP: {error}"),
            )
        })?;
    set_process(&state.processes, "php", child)?;
    sleep(Duration::from_millis(300)).await;
    if !process_running(&state.processes, "php") {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "PHP terminó al iniciar. Revisá {}.",
                state.data.join("logs").join("php-server.log").display()
            ),
        ));
    }
    Ok(Json(Message {
        message: format!("PHP está disponible en http://127.0.0.1:{}", request.port),
    }))
}

async fn stop_php(State(state): State<AppState>) -> ApiResult<Message> {
    stop_managed(&state.processes, "php")?;
    Ok(Json(Message {
        message: "Servidor PHP detenido.".into(),
    }))
}

async fn select_php(
    State(state): State<AppState>,
    Json(request): Json<SelectPhp>,
) -> ApiResult<Message> {
    if !valid_version(&request.version) {
        return Err((StatusCode::BAD_REQUEST, "Versión inválida.".into()));
    }
    let runtime = state
        .data
        .join("runtimes")
        .join("php")
        .join(&request.version);
    if !runtime.join("php.exe").is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            "Esa versión no está instalada.".into(),
        ));
    }
    fs::write(state.data.join("selected-php.txt"), request.version).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("No se pudo guardar la selección: {error}"),
        )
    })?;
    Ok(Json(Message {
        message: "Versión de PHP seleccionada para los nuevos servidores.".into(),
    }))
}

async fn select_mariadb(
    State(state): State<AppState>,
    Json(request): Json<SelectMariaDb>,
) -> ApiResult<Message> {
    if !valid_version(&request.version) {
        return Err((StatusCode::BAD_REQUEST, "Versión inválida.".into()));
    }
    let runtime = state
        .data
        .join("runtimes")
        .join("mariadb")
        .join(&request.version);
    if !runtime.join("bin").join("mariadb-install-db.exe").is_file()
        && !runtime.join("bin").join("mysql_install_db.exe").is_file()
    {
        return Err((
            StatusCode::NOT_FOUND,
            "Esa versión no está instalada o no incluye el inicializador para Windows.".into(),
        ));
    }
    fs::write(state.data.join("selected-mariadb.txt"), request.version).map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("No se pudo guardar la selección: {error}"),
        )
    })?;
    Ok(Json(Message {
        message: "Versión de MariaDB seleccionada; usa una carpeta de datos aislada.".into(),
    }))
}

async fn start_mysql(
    State(state): State<AppState>,
    Json(request): Json<StartMariaDb>,
) -> ApiResult<Message> {
    if !(1024..=65535).contains(&request.port) {
        return Err((StatusCode::BAD_REQUEST, "Puerto inválido.".into()));
    }
    if port_open(request.port) {
        return Err((
            StatusCode::CONFLICT,
            format!(
                "El puerto {} ya está en uso. Elegí otro puerto para no interferir con XAMPP u otro servicio.",
                request.port
            ),
        ));
    }
    if process_running(&state.processes, "mysql") {
        return Err((
            StatusCode::CONFLICT,
            "MariaDB ya está iniciado; detenelo antes de cambiar de instancia.".into(),
        ));
    }
    let version = selected_version(&state, "mariadb").ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            "Descargá e instalá una versión de MariaDB y seleccionala primero.".into(),
        )
    })?;
    let runtime = state.data.join("runtimes").join("mariadb").join(&version);
    let executable = ["mariadbd.exe", "mysqld.exe"]
        .iter()
        .map(|name| runtime.join("bin").join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "No encuentro el ejecutable del servidor MariaDB.".into(),
            )
        })?;
    let data_dir = state.data.join("databases").join(&version);
    fs::create_dir_all(&data_dir).map_err(api_error)?;
    let config = data_dir.join("my.ini");
    if !data_dir.join("mysql").is_dir() {
        initialize_mariadb(&runtime, &data_dir, request.port).await?;
    }
    if !config.is_file() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "La base de datos no tiene configuración {}. No se modificaron sus archivos.",
                config.display()
            ),
        ));
    }
    let (stdout, stderr) = log_streams(&state.data, "mysql").map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("No se pudo abrir el log de MySQL: {error}"),
        )
    })?;
    let child = Command::new(executable)
        .arg(format!("--defaults-file={}", config.display()))
        .arg("--console")
        .arg(format!("--port={}", request.port))
        .arg("--bind-address=127.0.0.1")
        .current_dir(&runtime)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!(
                    "No se pudo iniciar MariaDB. Puede faltar Visual C++ Redistributable: {error}"
                ),
            )
        })?;
    set_process(&state.processes, "mysql", child)?;
    set_mysql_config(&state.processes, &runtime.join("bin"), request.port)?;
    sleep(Duration::from_millis(300)).await;
    if !process_running(&state.processes, "mysql") {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "MariaDB terminó al iniciar. Revisá {}.",
                state.data.join("logs").join("mysql.log").display()
            ),
        ));
    }
    Ok(Json(Message {
        message: format!(
            "MariaDB {} está disponible en 127.0.0.1:{} (base independiente).",
            version, request.port
        ),
    }))
}

async fn stop_mysql(State(state): State<AppState>) -> ApiResult<Message> {
    if !process_running(&state.processes, "mysql") {
        return Err((
            StatusCode::CONFLICT,
            "El panel solo detiene MySQL/MariaDB que él mismo inició.".into(),
        ));
    }
    let (mysql_bin, port) = mysql_connection_settings(&state.processes).ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            "El panel perdió la configuración de MariaDB activa.".into(),
        )
    })?;
    let mysqladmin = ["mariadb-admin.exe", "mysqladmin.exe"]
        .iter()
        .map(|name| mysql_bin.join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "No encuentro mariadb-admin.exe para apagar la base normalmente.".into(),
            )
        })?;
    let output = Command::new(mysqladmin)
        .arg("--host=127.0.0.1")
        .arg(format!("--port={port}"))
        .arg("-u")
        .arg("root")
        .arg("shutdown")
        .current_dir(&mysql_bin)
        .output()
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("No se pudo solicitar el apagado de MySQL: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "MariaDB no aceptó el apagado normal. Revisá el log del servidor. {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    stop_managed(&state.processes, "mysql")?;
    Ok(Json(Message {
        message: "Se solicitó el apagado normal de MySQL/MariaDB.".into(),
    }))
}

async fn initialize_mariadb(runtime: &Path, data_dir: &Path, port: u16) -> ApiResult<()> {
    let bin = runtime.join("bin");
    let installer = ["mariadb-install-db.exe", "mysql_install_db.exe"]
        .iter()
        .map(|name| bin.join(name))
        .find(|path| path.is_file())
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "Este ZIP no contiene mariadb-install-db.exe; no se puede inicializar una base nueva.".into(),
            )
        })?;
    let data_name = data_dir
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "La carpeta de datos de MariaDB tiene un nombre inválido.".into(),
            )
        })?;
    let config_template = data_dir
        .parent()
        .unwrap_or(data_dir)
        .join(format!("{data_name}-xhamp-rust.ini"));
    let basedir = runtime.to_string_lossy().replace('\\', "/");
    let datadir = data_dir.to_string_lossy().replace('\\', "/");
    let template = format!(
        "[mysqld]\nbasedir=\"{basedir}\"\ndatadir=\"{datadir}\"\nport={port}\nbind-address=127.0.0.1\n"
    );
    fs::write(&config_template, template).map_err(api_error)?;
    let installer_data = data_dir.to_path_buf();
    let installer_config = config_template.clone();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(installer)
            .arg(format!("--datadir={}", installer_data.display()))
            .arg(format!("--port={port}"))
            .arg(format!("--config={}", installer_config.display()))
            .arg("--silent")
            .current_dir(bin)
            .output()
    })
    .await
    .map_err(api_error)?
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("No se pudo ejecutar el inicializador de MariaDB: {error}"),
        )
    })?;
    if !output.status.success() || !data_dir.join("mysql").is_dir() {
        let details = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let _ = fs::remove_file(&config_template);
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "No se pudo inicializar la base vacía de MariaDB. No se borraron archivos. {} {} Revisá {}.",
                details.trim(),
                stdout.trim(),
                data_dir.display()
            ),
        ));
    }
    let _ = fs::remove_file(config_template);
    Ok(())
}

fn set_mysql_config(
    processes: &Arc<Mutex<Processes>>,
    mysql_bin: &Path,
    port: u16,
) -> Result<(), (StatusCode, String)> {
    let mut processes = processes
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Estado ocupado.".into()))?;
    processes.mysql_bin = Some(mysql_bin.to_path_buf());
    processes.mysql_port = Some(port);
    Ok(())
}

fn mysql_connection_settings(processes: &Arc<Mutex<Processes>>) -> Option<(PathBuf, u16)> {
    let processes = processes.lock().ok()?;
    Some((processes.mysql_bin.clone()?, processes.mysql_port?))
}

async fn install_release(
    State(state): State<AppState>,
    Json(request): Json<InstallRelease>,
) -> ApiResult<Message> {
    let _install_guard = state.install_lock.lock().await;
    if !valid_version(&request.version) || !matches!(request.kind.as_str(), "php" | "mariadb") {
        return Err((
            StatusCode::BAD_REQUEST,
            "Paquete o versión inválidos.".into(),
        ));
    }
    let catalog = if request.kind == "php" {
        php_releases(&state.client).await.map_err(api_error)?
    } else {
        mariadb_releases(&state.client).await.map_err(api_error)?
    };
    let release = catalog
        .into_iter()
        .find(|release| release.version == request.version)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                "La versión ya no aparece en el catálogo oficial.".into(),
            )
        })?;
    let url = reqwest::Url::parse(&release.download_url).map_err(api_error)?;
    let allowed_host = match request.kind.as_str() {
        "php" => matches!(
            url.host_str(),
            Some("windows.php.net") | Some("downloads.php.net")
        ),
        _ => url.host_str() == Some("downloads.mariadb.org"),
    };
    if url.scheme() != "https" || !allowed_host {
        return Err((
            StatusCode::BAD_REQUEST,
            "El catálogo devolvió una URL de descarga no confiable.".into(),
        ));
    }

    let runtime_root = state.data.join("runtimes").join(&request.kind);
    let destination = runtime_root.join(&release.version);
    if destination.exists() {
        return Err((
            StatusCode::CONFLICT,
            "Esa versión ya existe; no se sobrescribió ningún archivo.".into(),
        ));
    }
    let downloads = state.data.join("downloads");
    let archive_path = downloads.join(format!("{}-{}.zip", request.kind, release.version));
    let partial_path = downloads.join(format!("{}-{}.zip.part", request.kind, release.version));
    let mut response = state
        .client
        .get(&release.download_url)
        .send()
        .await
        .map_err(api_error)?;
    if !response.status().is_success() {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!("La descarga respondió con HTTP {}.", response.status()),
        ));
    }
    if response
        .content_length()
        .map(|length| length > MAX_DOWNLOAD_BYTES)
        .unwrap_or(false)
    {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            "El archivo supera el límite de descarga de 512 MiB.".into(),
        ));
    }
    let mut file = tokio::fs::File::create(&partial_path)
        .await
        .map_err(api_error)?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    while let Some(chunk) = response.chunk().await.map_err(api_error)? {
        total = total.saturating_add(chunk.len() as u64);
        if total > MAX_DOWNLOAD_BYTES {
            let _ = tokio::fs::remove_file(&partial_path).await;
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                "El archivo supera el límite de descarga de 512 MiB.".into(),
            ));
        }
        hasher.update(&chunk);
        file.write_all(&chunk).await.map_err(api_error)?;
    }
    file.flush().await.map_err(api_error)?;
    drop(file);
    let actual_hash = format!("{:x}", hasher.finalize());
    if !actual_hash.eq_ignore_ascii_case(&release.sha256) {
        let _ = tokio::fs::remove_file(&partial_path).await;
        return Err((
            StatusCode::BAD_GATEWAY,
            "El SHA-256 no coincide con el catálogo oficial; se descartó la descarga.".into(),
        ));
    }
    tokio::fs::rename(&partial_path, &archive_path)
        .await
        .map_err(api_error)?;

    let staging = runtime_root.join(format!("{}.installing", release.version));
    let archive_for_extract = archive_path.clone();
    let staging_for_extract = staging.clone();
    let kind = request.kind.clone();
    let is_mariadb = kind == "mariadb";
    let extracted = tokio::task::spawn_blocking(move || {
        extract_archive(&archive_for_extract, &staging_for_extract)?;
        if is_mariadb {
            normalize_mariadb_root(&staging_for_extract)?;
        }
        Ok::<(), String>(())
    })
    .await
    .map_err(api_error)?;
    if let Err(error) = extracted {
        return Err(api_error(error));
    }
    let executable = match kind.as_str() {
        "php" => staging.join("php.exe").is_file(),
        _ => {
            (staging.join("bin").join("mariadbd.exe").is_file()
                || staging.join("bin").join("mysqld.exe").is_file())
                && (staging.join("bin").join("mariadb-install-db.exe").is_file()
                    || staging.join("bin").join("mysql_install_db.exe").is_file())
        }
    };
    if !executable {
        return Err((
            StatusCode::BAD_GATEWAY,
            "El ZIP no contiene el ejecutable esperado; se conservó el archivo original para inspección.".into(),
        ));
    }
    tokio::fs::rename(&staging, &destination)
        .await
        .map_err(api_error)?;
    if kind == "php" {
        let php_root = destination.clone();
        tokio::task::spawn_blocking(move || configure_php_runtime(&php_root))
            .await
            .map_err(api_error)?
            .map_err(api_error)?;
        let output = Command::new(destination.join("php.exe"))
            .arg("-c")
            .arg(destination.join("php.ini"))
            .arg("--version")
            .output()
            .map_err(|error| {
                api_error(format!(
                    "PHP quedó descargado, pero Windows no pudo ejecutarlo. Instalá Microsoft Visual C++ Redistributable x64: {error}"
                ))
            })?;
        if !output.status.success() {
            return Err(api_error(format!(
                "PHP quedó descargado, pero no pudo iniciarse. Puede faltar Microsoft Visual C++ Redistributable x64: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
    }

    Ok(Json(Message {
        message: if kind == "php" {
            format!(
                "PHP {} quedó instalado y configurado sin modificar XAMPP. Seleccionalo para usarlo.",
                release.version
            )
        } else {
            format!(
                "MariaDB {} quedó instalado en una carpeta aislada. Seleccionalo y el panel creará una base nueva e independiente.",
                release.version
            )
        },
    }))
}

fn configure_php_runtime(root: &Path) -> Result<(), String> {
    let template = root.join("php.ini-development");
    if !template.is_file() {
        return Err("El paquete de PHP no contiene php.ini-development.".into());
    }
    let mut config = fs::read_to_string(&template).map_err(|error| error.to_string())?;
    let extension_dir = root.join("ext").to_string_lossy().replace('\\', "/");
    config.push_str(&format!(
        "\n\n; Xhamp Rust local development defaults\nextension_dir = \"{extension_dir}\"\ndate.timezone = America/Argentina/Buenos_Aires\nlog_errors = On\n"
    ));
    for extension in [
        "curl",
        "fileinfo",
        "mbstring",
        "mysqli",
        "openssl",
        "pdo_mysql",
        "sodium",
        "zip",
    ] {
        config.push_str(&format!("extension={extension}\n"));
    }
    fs::write(root.join("php.ini"), config).map_err(|error| error.to_string())
}

async fn php_releases(client: &Client) -> Result<Vec<Release>, String> {
    let catalog: Value = client
        .get(PHP_CATALOG_URL)
        .send()
        .await
        .map_err(|error| format!("No se pudo consultar el catálogo de PHP: {error}"))?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| format!("El catálogo de PHP no devolvió JSON válido: {error}"))?;

    let Some(branches) = catalog.as_object() else {
        return Err("Formato inesperado del catálogo de PHP.".into());
    };
    let mut releases = BTreeMap::new();
    for (branch, entry) in branches {
        if !branch.starts_with("8.") || branch == "8.0" {
            continue;
        }
        let Some(version) = entry.get("version").and_then(Value::as_str) else {
            continue;
        };
        let Some(packages) = entry.as_object() else {
            continue;
        };
        let mut candidates: Vec<_> = packages
            .iter()
            .filter(|(key, _)| key.starts_with("ts-") && key.ends_with("x64"))
            .filter_map(|(key, package)| {
                let zip = package.get("zip")?;
                let path = zip.get("path")?.as_str()?;
                let sha256 = zip.get("sha256")?.as_str()?;
                Some((
                    if key.contains("vs17") { 0 } else { 1 },
                    path.to_string(),
                    sha256.to_string(),
                ))
            })
            .collect();
        candidates.sort_by_key(|item| item.0);
        if let Some((_, path, sha256)) = candidates.into_iter().next() {
            releases.insert(
                branch.clone(),
                Release {
                    version: version.to_string(),
                    download_url: format!("https://windows.php.net/downloads/releases/{path}"),
                    sha256,
                },
            );
        }
    }
    Ok(releases.into_values().rev().collect())
}

async fn mariadb_releases(client: &Client) -> Result<Vec<Release>, String> {
    let catalog: Value = client
        .get(MARIADB_CATALOG_URL)
        .send()
        .await
        .map_err(|error| format!("No se pudo consultar el catálogo de MariaDB: {error}"))?
        .error_for_status()
        .map_err(|error| error.to_string())?
        .json()
        .await
        .map_err(|error| format!("El catálogo de MariaDB no devolvió JSON válido: {error}"))?;
    let majors = catalog
        .get("major_releases")
        .and_then(Value::as_array)
        .ok_or_else(|| "Formato inesperado del catálogo de MariaDB.".to_string())?;
    let mut releases = Vec::new();
    for major in majors {
        if major.get("release_status").and_then(Value::as_str) != Some("Stable") {
            continue;
        }
        let Some(major_id) = major.get("release_id").and_then(Value::as_str) else {
            continue;
        };
        let url = format!("{MARIADB_CATALOG_URL}{major_id}/");
        let details: Value = client
            .get(url)
            .send()
            .await
            .map_err(|error| format!("No se pudo consultar MariaDB {major_id}: {error}"))?
            .error_for_status()
            .map_err(|error| error.to_string())?
            .json()
            .await
            .map_err(|error| error.to_string())?;
        let Some(version) = details
            .get("releases")
            .and_then(Value::as_object)
            .and_then(|map| {
                map.values().max_by_key(|release| {
                    release
                        .get("date_of_release")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                })
            })
        else {
            continue;
        };
        let version_name = version
            .get("release_id")
            .and_then(Value::as_str)
            .ok_or_else(|| "La versión MariaDB no tiene identificador.".to_string())?;
        let files = version
            .get("files")
            .and_then(Value::as_array)
            .ok_or_else(|| "La versión MariaDB no tiene lista de archivos.".to_string())?;
        if let Some(file) = files.iter().find(|file| {
            file.get("file_name")
                .and_then(Value::as_str)
                .map(|name| name.ends_with("-winx64.zip") && !name.contains("debug"))
                .unwrap_or(false)
        }) {
            let url = file
                .get("file_download_url")
                .and_then(Value::as_str)
                .ok_or_else(|| "El paquete MariaDB no tiene URL.".to_string())?
                .replace("http://", "https://");
            let sha256 = file
                .get("checksum")
                .and_then(|value| value.get("sha256sum"))
                .and_then(Value::as_str)
                .ok_or_else(|| "El paquete MariaDB no tiene SHA-256.".to_string())?;
            releases.push(Release {
                version: version_name.to_string(),
                download_url: url,
                sha256: sha256.to_string(),
            });
        }
    }
    releases.sort_by(|left, right| right.version.cmp(&left.version));
    Ok(releases)
}

fn extract_archive(archive_path: &Path, destination: &Path) -> Result<(), String> {
    fs::create_dir(destination).map_err(|error| error.to_string())?;
    let result = (|| {
        let file = File::open(archive_path).map_err(|error| error.to_string())?;
        let mut archive = ZipArchive::new(file).map_err(|error| error.to_string())?;
        let mut expanded = 0u64;
        for index in 0..archive.len() {
            let mut entry = archive.by_index(index).map_err(|error| error.to_string())?;
            let relative = entry
                .enclosed_name()
                .ok_or_else(|| "El ZIP contiene una ruta no segura.".to_string())?
                .to_path_buf();
            if relative.components().any(|component| {
                matches!(
                    component,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            }) || entry.is_symlink()
            {
                return Err("El ZIP contiene una ruta o enlace no permitido.".into());
            }
            let target = destination.join(&relative);
            if entry.is_dir() {
                fs::create_dir_all(&target).map_err(|error| error.to_string())?;
                continue;
            }
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent).map_err(|error| error.to_string())?;
            }
            let mut output = File::create(&target).map_err(|error| error.to_string())?;
            let remaining = MAX_EXPANDED_BYTES.saturating_sub(expanded);
            let copied = io::copy(&mut entry.take(remaining.saturating_add(1)), &mut output)
                .map_err(|error| error.to_string())?;
            expanded = expanded.saturating_add(copied);
            if expanded > MAX_EXPANDED_BYTES {
                return Err("El contenido extraído excede el límite de 2 GiB.".into());
            }
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(destination);
    }
    result
}

fn normalize_mariadb_root(destination: &Path) -> Result<(), String> {
    let mut entries = fs::read_dir(destination)
        .map_err(|error| error.to_string())?
        .filter_map(Result::ok);
    let Some(root) = entries.next().map(|entry| entry.path()) else {
        return Err("El archivo MariaDB está vacío.".into());
    };
    if entries.next().is_some()
        || !root.is_dir()
        || !(root.join("bin").join("mariadbd.exe").is_file()
            || root.join("bin").join("mysqld.exe").is_file())
    {
        return Ok(());
    }
    for entry in fs::read_dir(&root).map_err(|error| error.to_string())? {
        let entry = entry.map_err(|error| error.to_string())?;
        fs::rename(entry.path(), destination.join(entry.file_name()))
            .map_err(|error| error.to_string())?;
    }
    fs::remove_dir(root).map_err(|error| error.to_string())
}

fn selected_version(state: &AppState, kind: &str) -> Option<String> {
    let root = state.data.join("runtimes").join(kind);
    let versions = installed_versions(&root, kind);
    let selected_file = state.data.join(format!("selected-{kind}.txt"));
    if let Ok(selected) = fs::read_to_string(selected_file) {
        let selected = selected.trim();
        if valid_version(selected) && versions.iter().any(|version| version == selected) {
            return Some(selected.to_string());
        }
    }
    versions.into_iter().last()
}

fn selected_php_path(state: &AppState) -> Option<PathBuf> {
    Some(
        state
            .data
            .join("runtimes")
            .join("php")
            .join(selected_version(state, "php")?)
            .join("php.exe"),
    )
}

fn version_cmp(left: &str, right: &str) -> std::cmp::Ordering {
    let left_parts: Vec<_> = left.split(['.', '-']).collect();
    let right_parts: Vec<_> = right.split(['.', '-']).collect();
    for (left, right) in left_parts.iter().zip(right_parts.iter()) {
        let ordering = match (left.parse::<u64>(), right.parse::<u64>()) {
            (Ok(left), Ok(right)) => left.cmp(&right),
            _ => left.cmp(right),
        };
        if ordering != std::cmp::Ordering::Equal {
            return ordering;
        }
    }
    left_parts.len().cmp(&right_parts.len())
}

fn valid_project_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 48
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
}

fn valid_version(version: &str) -> bool {
    !version.is_empty()
        && version.len() <= 32
        && version
            .chars()
            .all(|character| character.is_ascii_digit() || matches!(character, '.' | '-'))
}

fn list_projects(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut projects: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    projects.sort();
    projects
}

fn executable_version(executable: &Path, argument: &str) -> String {
    let Ok(output) = Command::new(executable).arg(argument).output() else {
        return "no detectado".into();
    };
    let output = String::from_utf8_lossy(&output.stdout);
    output
        .lines()
        .next()
        .unwrap_or("no detectado")
        .trim()
        .to_string()
}

fn log_streams(data: &Path, name: &str) -> io::Result<(Stdio, Stdio)> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(data.join("logs").join(format!("{name}.log")))?;
    let stdout = Stdio::from(file.try_clone()?);
    let stderr = Stdio::from(file);
    Ok((stdout, stderr))
}

fn port_open(port: u16) -> bool {
    TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        Duration::from_millis(150),
    )
    .is_ok()
}

fn process_running(processes: &Arc<Mutex<Processes>>, name: &str) -> bool {
    let Ok(mut processes) = processes.lock() else {
        return false;
    };
    let is_mysql = name == "mysql";
    let status = if is_mysql {
        processes.mysql.as_mut().map(Child::try_wait)
    } else {
        processes.php.as_mut().map(Child::try_wait)
    };
    match status {
        Some(Ok(Some(_))) => {
            if is_mysql {
                processes.mysql = None;
                processes.mysql_bin = None;
                processes.mysql_port = None;
            } else {
                processes.php = None;
            }
            false
        }
        Some(Ok(None)) => true,
        Some(Err(_)) => false,
        None => false,
    }
}

fn set_process(
    processes: &Arc<Mutex<Processes>>,
    name: &str,
    child: Child,
) -> Result<(), (StatusCode, String)> {
    let mut processes = processes
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Estado ocupado.".into()))?;
    let slot = if name == "mysql" {
        &mut processes.mysql
    } else {
        &mut processes.php
    };
    if slot.is_some() {
        let mut child = child;
        let _ = child.kill();
        let _ = child.wait();
        return Err((
            StatusCode::CONFLICT,
            format!("{name} ya está iniciado por el panel."),
        ));
    }
    *slot = Some(child);
    Ok(())
}

fn stop_managed(processes: &Arc<Mutex<Processes>>, name: &str) -> Result<(), (StatusCode, String)> {
    let mut processes = processes
        .lock()
        .map_err(|_| (StatusCode::INTERNAL_SERVER_ERROR, "Estado ocupado.".into()))?;
    let is_mysql = name == "mysql";
    let mut child = if is_mysql {
        processes.mysql.take()
    } else {
        processes.php.take()
    };
    if let Some(mut child) = child.take() {
        for _ in 0..30 {
            match child.try_wait() {
                Ok(Some(_)) => {
                    if is_mysql {
                        processes.mysql_bin = None;
                        processes.mysql_port = None;
                    }
                    return Ok(());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(error) => {
                    if is_mysql {
                        processes.mysql = Some(child);
                    } else {
                        processes.php = Some(child);
                    }
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("No se pudo verificar el apagado: {error}"),
                    ));
                }
            }
        }
        if is_mysql {
            processes.mysql = Some(child);
        } else {
            processes.php = Some(child);
        }
        return Err((
            StatusCode::GATEWAY_TIMEOUT,
            format!("{name} sigue activo; no se forzó su cierre."),
        ));
    }
    Ok(())
}

fn api_error(error: impl std::fmt::Display) -> (StatusCode, String) {
    (StatusCode::BAD_GATEWAY, error.to_string())
}

#[cfg(target_os = "windows")]
fn open_browser(url: &str) -> io::Result<()> {
    Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn()
        .map(|_| ())
}

#[cfg(target_os = "macos")]
fn open_browser(url: &str) -> io::Result<()> {
    Command::new("open").arg(url).spawn().map(|_| ())
}

#[cfg(all(unix, not(target_os = "macos")))]
fn open_browser(url: &str) -> io::Result<()> {
    Command::new("xdg-open").arg(url).spawn().map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::{valid_project_name, valid_version, version_cmp};

    #[test]
    fn project_names_are_limited_to_safe_directory_names() {
        assert!(valid_project_name("mi_sitio-2"));
        assert!(!valid_project_name(""));
        assert!(!valid_project_name("../sitio"));
        assert!(!valid_project_name("sitio web"));
    }

    #[test]
    fn version_names_cannot_contain_path_components() {
        assert!(valid_version("8.4.27-1"));
        assert!(!valid_version("../8.4"));
        assert!(!valid_version("8.4\\php.exe"));
    }

    #[test]
    fn versions_are_sorted_numerically_not_lexically() {
        assert!(version_cmp("8.10.0", "8.9.9").is_gt());
        assert!(version_cmp("11.8.9", "10.11.20").is_gt());
        assert!(version_cmp("8.4.27-1", "8.4.27-2").is_lt());
    }

    #[test]
    fn generated_php_project_has_a_valid_safe_project_name() {
        let project_name = "mi_sitio-2";
        let template = include_str!("../static/project-index.php");
        let generated = template
            .replace("__PROJECT_NAME__", project_name)
            .replace("\r\n", "\n");

        assert!(generated.starts_with("<?php\n$projectName = 'mi_sitio-2';"));
        assert!(!generated.contains("__PROJECT_NAME__"));
        assert!(generated
            .contains("htmlspecialchars($projectName, ENT_QUOTES | ENT_SUBSTITUTE, 'UTF-8')"));
    }
}

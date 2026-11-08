use axum::{
    extract::{Path as AxumPath, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures_util::StreamExt;
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
const PHP_CATALOG_URL: &str = "https://windows.php.net/downloads/releases/releases.json";
const MARIADB_CATALOG_URL: &str = "https://downloads.mariadb.org/rest-api/mariadb/";
const MAX_DOWNLOAD_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXPANDED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

#[derive(Clone)]
struct AppState {
    xampp: PathBuf,
    data: PathBuf,
    php_version: String,
    mysql_version: String,
    client: Client,
    processes: Arc<Mutex<Processes>>,
    install_lock: Arc<tokio::sync::Mutex<()>>,
}

fn installed_versions(root: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut versions: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(|entry| entry.path().join("php.exe").is_file())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .collect();
    versions.sort();
    versions
}

#[derive(Default)]
struct Processes {
    apache: Option<Child>,
    mysql: Option<Child>,
    php: Option<Child>,
}

#[derive(Serialize)]
struct Status {
    xampp_path: String,
    xampp_found: bool,
    php_version: String,
    mysql_version: String,
    apache: String,
    mysql: String,
    php: String,
    selected_php: String,
    installed_php: Vec<String>,
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

    let xampp = env::var_os("XAMPP_HOME")
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .or_else(|| {
            [PathBuf::from(r"C:\xampp"), PathBuf::from(r"C:\XAMPP")]
                .into_iter()
                .find(|path| path.join("php").join("php.exe").is_file())
        })
        .unwrap_or_else(|| PathBuf::from(r"C:\xampp"));

    let client = Client::builder()
        .user_agent(concat!("XhampRust/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(15 * 60))
        .build()?;
    let php_version = executable_version(&xampp.join("php").join("php.exe"), "--version");
    let mysql_version = executable_version(
        &xampp.join("mysql").join("bin").join("mysqld.exe"),
        "--version",
    );
    let state = AppState {
        xampp,
        data,
        php_version,
        mysql_version,
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
        .route("/api/apache/start", post(start_apache))
        .route("/api/apache/stop", post(stop_apache))
        .route("/api/mysql/start", post(start_mysql))
        .route("/api/mysql/stop", post(stop_mysql))
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
    let apache_running = process_running(&state.processes, "apache");
    let mysql_managed = process_running(&state.processes, "mysql");
    let php_running = process_running(&state.processes, "php");
    let projects = list_projects(&state.data.join("projects"));
    let selected_php = selected_php_path(&state)
        .and_then(|path| version_from_path(&path))
        .unwrap_or_else(|| "XAMPP".to_string());

    Ok(Json(Status {
        xampp_path: state.xampp.display().to_string(),
        xampp_found: state.xampp.join("php").join("php.exe").is_file(),
        php_version: state.php_version.clone(),
        mysql_version: state.mysql_version.clone(),
        apache: if apache_running {
            "iniciado por el panel".to_string()
        } else if port_open(80) {
            "puerto 80 ocupado (no gestionado)".to_string()
        } else {
            "detenido".to_string()
        },
        mysql: if mysql_managed || port_open(3306) {
            if mysql_managed {
                "iniciado por el panel".to_string()
            } else {
                "puerto 3306 ocupado (no gestionado)".to_string()
            }
        } else {
            "detenido".to_string()
        },
        php: if php_running {
            "servidor iniciado".to_string()
        } else {
            "detenido".to_string()
        },
        selected_php,
        installed_php: installed_versions(&state.data.join("runtimes").join("php")),
        projects,
    }))
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

    let php = selected_php_path(&state).unwrap_or_else(|| state.xampp.join("php").join("php.exe"));
    if !php.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            "No encuentro php.exe. Configurá XAMPP_HOME o instalá una versión de PHP.".into(),
        ));
    }
    let mut command = Command::new(&php);
    if php.starts_with(state.data.join("runtimes").join("php")) {
        command
            .arg("-d")
            .arg(format!(
                "extension_dir={}",
                php.parent().unwrap_or(Path::new(".")).join("ext").display()
            ))
            .arg("-d")
            .arg("extension=mysqli")
            .arg("-d")
            .arg("extension=pdo_mysql");
    }
    let (stdout, stderr) = log_streams(&state.data, "php-server").map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("No se pudo abrir el log de PHP: {error}"),
        )
    })?;
    let child = command
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

async fn start_apache(State(state): State<AppState>) -> ApiResult<Message> {
    if port_open(80) {
        return Err((
            StatusCode::CONFLICT,
            "El puerto 80 ya está en uso. No voy a cerrar procesos ajenos.".into(),
        ));
    }
    let apache = state.xampp.join("apache");
    let executable = apache.join("bin").join("httpd.exe");
    let config = apache.join("conf").join("httpd.conf");
    if !executable.is_file() || !config.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            "No encuentro Apache de XAMPP.".into(),
        ));
    }
    let (stdout, stderr) = log_streams(&state.data, "apache").map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("No se pudo abrir el log de Apache: {error}"),
        )
    })?;
    let child = Command::new(executable)
        .arg("-f")
        .arg(config)
        .current_dir(&apache)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("No se pudo iniciar Apache: {error}"),
            )
        })?;
    set_process(&state.processes, "apache", child)?;
    sleep(Duration::from_millis(300)).await;
    if !process_running(&state.processes, "apache") {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "Apache terminó al iniciar. Revisá {}.",
                state.data.join("logs").join("apache.log").display()
            ),
        ));
    }
    Ok(Json(Message {
        message: "Apache está iniciando en el puerto 80.".into(),
    }))
}

async fn stop_apache(State(state): State<AppState>) -> ApiResult<Message> {
    let managed = process_running(&state.processes, "apache");
    if !managed {
        return Err((
            StatusCode::CONFLICT,
            "El panel solo detiene el Apache que él mismo inició.".into(),
        ));
    }
    let apache = state.xampp.join("apache");
    let output = Command::new(apache.join("bin").join("httpd.exe"))
        .arg("-k")
        .arg("stop")
        .arg("-f")
        .arg(apache.join("conf").join("httpd.conf"))
        .current_dir(apache)
        .output()
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("No se pudo detener Apache: {error}"),
            )
        })?;
    if !output.status.success() {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!(
                "Apache no aceptó el apagado: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    stop_managed(&state.processes, "apache")?;
    Ok(Json(Message {
        message: "Se solicitó el apagado normal de Apache.".into(),
    }))
}

async fn start_mysql(State(state): State<AppState>) -> ApiResult<Message> {
    if port_open(3306) {
        return Err((
            StatusCode::CONFLICT,
            "El puerto 3306 ya está en uso. No voy a cerrar procesos ajenos.".into(),
        ));
    }
    let executable = state.xampp.join("mysql").join("bin").join("mysqld.exe");
    let config = state.xampp.join("mysql").join("bin").join("my.ini");
    if !executable.is_file() || !config.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            "No encuentro MariaDB/MySQL y su configuración en XAMPP.".into(),
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
        .arg("--standalone")
        .current_dir(&state.xampp)
        .stdout(stdout)
        .stderr(stderr)
        .spawn()
        .map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("No se pudo iniciar MySQL/MariaDB: {error}"),
            )
        })?;
    set_process(&state.processes, "mysql", child)?;
    sleep(Duration::from_millis(300)).await;
    if !process_running(&state.processes, "mysql") {
        return Err((
            StatusCode::BAD_GATEWAY,
            format!(
                "MySQL/MariaDB terminó al iniciar. Revisá {}.",
                state.data.join("logs").join("mysql.log").display()
            ),
        ));
    }
    Ok(Json(Message {
        message: "MySQL/MariaDB está iniciando en el puerto 3306.".into(),
    }))
}

async fn stop_mysql(State(state): State<AppState>) -> ApiResult<Message> {
    if !process_running(&state.processes, "mysql") {
        return Err((
            StatusCode::CONFLICT,
            "El panel solo detiene MySQL/MariaDB que él mismo inició.".into(),
        ));
    }
    let mysqladmin = state.xampp.join("mysql").join("bin").join("mysqladmin.exe");
    let output = Command::new(mysqladmin)
        .arg("-u")
        .arg("root")
        .arg("shutdown")
        .current_dir(&state.xampp)
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
                "MySQL no aceptó el apagado normal. Si root tiene contraseña, revisá el log de XAMPP. {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
        ));
    }
    stop_managed(&state.processes, "mysql")?;
    Ok(Json(Message {
        message: "Se solicitó el apagado normal de MySQL/MariaDB.".into(),
    }))
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
            staging.join("bin").join("mariadbd.exe").is_file()
                || staging.join("bin").join("mysqld.exe").is_file()
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

    Ok(Json(Message {
        message: if kind == "php" {
            format!(
                "PHP {} quedó instalado en una carpeta aislada. Seleccionalo para usarlo.",
                release.version
            )
        } else {
            format!(
                "MariaDB {} quedó descargado y extraído, pero no se activó ni modificó la base existente.",
                release.version
            )
        },
    }))
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

fn selected_php_path(state: &AppState) -> Option<PathBuf> {
    let version = fs::read_to_string(state.data.join("selected-php.txt")).ok()?;
    let version = version.trim();
    if !valid_version(version) {
        return None;
    }
    let executable = state
        .data
        .join("runtimes")
        .join("php")
        .join(version)
        .join("php.exe");
    executable.is_file().then_some(executable)
}

fn version_from_path(path: &Path) -> Option<String> {
    path.parent()?.file_name()?.to_str().map(str::to_string)
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
    let process = match name {
        "apache" => &mut processes.apache,
        "mysql" => &mut processes.mysql,
        _ => &mut processes.php,
    };
    match process {
        Some(child) => match child.try_wait() {
            Ok(Some(_)) => {
                *process = None;
                false
            }
            Ok(None) => true,
            Err(_) => false,
        },
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
    let slot = match name {
        "apache" => &mut processes.apache,
        "mysql" => &mut processes.mysql,
        _ => &mut processes.php,
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
    let process = match name {
        "apache" => &mut processes.apache,
        "mysql" => &mut processes.mysql,
        _ => &mut processes.php,
    };
    if let Some(mut child) = process.take() {
        for _ in 0..30 {
            match child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
                Err(error) => {
                    *process = Some(child);
                    return Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        format!("No se pudo verificar el apagado: {error}"),
                    ));
                }
            }
        }
        *process = Some(child);
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
    use super::{valid_project_name, valid_version};

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
    fn generated_php_project_has_a_valid_safe_project_name() {
        let project_name = "mi_sitio-2";
        let template = include_str!("../static/project-index.php");
        let generated = template.replace("__PROJECT_NAME__", project_name);

        assert!(generated.starts_with("<?php\n$projectName = 'mi_sitio-2';"));
        assert!(!generated.contains("__PROJECT_NAME__"));
        assert!(generated
            .contains("htmlspecialchars($projectName, ENT_QUOTES | ENT_SUBSTITUTE, 'UTF-8')"));
    }
}

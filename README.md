# Xhamp Rust

Aplicación de escritorio para tener tu propio entorno PHP y MariaDB en Windows, independiente de XAMPP. Guarda versiones, sitios y bases en `%LOCALAPPDATA%\XhampRust`; nunca modifica los ejecutables ni las bases de XAMPP.

## Usar la app portable

1. Para probar una compilación, entra en **Actions**, selecciona **Build Windows portable app** y presiona **Run workflow** en `main`. Al terminar, descarga el artefacto `XhampRust-windows-x64`.
2. Para publicar una beta, crea y sube un tag, por ejemplo `v0.1.0-beta.1`. Actions compila para Windows x64 y adjunta `XhampRust-windows-x64.zip` y su archivo `.sha256` a **Releases**; los tags con guion se marcan como pre-release.
3. En Windows 10/11 x64, extrae el ZIP y ejecuta `XhampRust.exe`. El panel y la vista del sitio se muestran dentro de una ventana de Xhamp Rust, no en una pestaña del navegador. Deja abierto el programa mientras uses los servicios.

El ZIP contiene la aplicación; PHP y MariaDB se descargan desde sus catálogos oficiales dentro del panel cuando los necesites. La ventana requiere Microsoft Edge WebView2 Runtime, incluido normalmente con Windows 11 y Microsoft Edge. Si falta, Xhamp Rust muestra un aviso. Esta versión está preparada para Windows x64.

## Primer uso

1. En **Versiones**, descarga una versión de PHP y una de MariaDB.
2. Elige la versión y presiona **Usar esta versión**.
3. Crea un proyecto y presiona **Iniciar sitio PHP**. El sitio aparece en la vista integrada de la ventana.
4. Presiona **Iniciar MariaDB** para inicializar una base de datos vacía propia del programa.
5. Conecta tu proyecto a `127.0.0.1`, puerto `3307`, usuario `root` y contraseña vacía. Si tu XAMPP ocupa 3307, elegí otro puerto disponible; el valor se cambia antes de iniciar la base.
6. Usa los botones de detener para apagar normalmente los servicios.

La carpeta `databases\<version>` es propia de esa versión de MariaDB. Cambiar la versión seleccionada crea/usa otra carpeta; **no** reutiliza ni convierte datos de XAMPP o de otra versión automáticamente. La importación de XAMPP copia los sitios de `htdocs` y nunca sobrescribe destinos existentes; no importa bases de datos. Para migrar una base, expórtala con `mysqldump` y luego impórtala en la instancia nueva siguiendo el procedimiento oficial de MariaDB.

## Qué es y qué no es

- Ejecuta PHP con el servidor web de desarrollo incluido en PHP, escuchando solo en `127.0.0.1`.
- Ejecuta MariaDB en `127.0.0.1` con datos separados por versión.
- El puerto predeterminado de MariaDB es **3307**, para poder convivir con XAMPP, que suele usar 3306.
- Las versiones se descargan de fuentes oficiales, se verifican por SHA-256 y quedan lado a lado. Instalar o seleccionar PHP nuevo no reemplaza ni desinstala las versiones previas.
- El programa **no instala Apache**, no incluye phpMyAdmin y no configura correo/Mercury. El servidor de desarrollo de PHP es para desarrollo local y no es un servidor de producción ni admite carga de alta concurrencia.
- Los logs de PHP y MariaDB se muestran en la consola integrada y también se guardan en la carpeta `logs`.
- La base local se limita a loopback y usa `root` sin contraseña para desarrollo, igual que una instalación XAMPP sin asegurar. No uses esta configuración en una red compartida o en producción.

Podés conservar XAMPP y usar ambos en paralelo; Xhamp Rust no lee ni modifica sus archivos. Cuando estés listo para dejarlo, migra cada proyecto y exporta/importa sus bases por separado. Desinstalar XAMPP queda a tu decisión y no es necesario para usar esta aplicación.

## Compilar localmente

Requisitos:

- Windows 10/11 x64.
- Rust estable (`rustup` y `cargo`).
- Visual Studio Build Tools con **Desktop development with C++** y Windows SDK (en destino MSVC).
- Microsoft Edge WebView2 Runtime.
- Microsoft Visual C++ Redistributable x64 para los binarios oficiales de PHP y MariaDB.

En PowerShell, desde la carpeta del proyecto:

```powershell
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
```

El programa queda en `target\release\xhamp-rust.exe`. También se puede generar un paquete portable ejecutando:

```powershell
New-Item -ItemType Directory -Force dist | Out-Null
Copy-Item target\release\xhamp-rust.exe dist\XhampRust.exe
Copy-Item README.md dist\README.txt
Compress-Archive -Path dist\XhampRust.exe,dist\README.txt -DestinationPath XhampRust-windows-x64.zip -Force
```

El workflow `.github/workflows/build-windows.yml` ejecuta formato, tests y build para Windows; adjunta el ZIP portable a cada ejecución de Actions (artefacto temporal de 30 días).

## Carpetas y logs

- `%LOCALAPPDATA%\XhampRust\runtimes\php\<version>`: versiones de PHP.
- `%LOCALAPPDATA%\XhampRust\runtimes\mariadb\<version>`: servidores MariaDB.
- `%LOCALAPPDATA%\XhampRust\databases\<version>`: datos independientes por versión.
- `%LOCALAPPDATA%\XhampRust\htdocs`: sitios nuevos e importados. Los proyectos de la carpeta antigua `projects` se copian aquí al iniciar; la carpeta original se conserva.
- `%LOCALAPPDATA%\XhampRust\logs`: logs de PHP y MariaDB.

El panel y ambos servidores solo escuchan en localhost. Si el puerto elegido ya está ocupado, el panel lo informa y no termina procesos externos.

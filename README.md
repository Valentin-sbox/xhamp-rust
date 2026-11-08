# Xhamp Rust

Panel local, liviano y portable para administrar una instalacion existente de XAMPP en Windows. La interfaz abre en el navegador; el panel solo escucha en `127.0.0.1:8765`.

## Usar la aplicacion lista (Windows x64)

No necesitas instalar Rust ni Visual Studio para usar la version portable:

1. En GitHub, abre la pestana **Actions** y selecciona **Build Windows portable app**.
2. Presiona **Run workflow** en la rama `main`.
3. Cuando termine la ejecucion, descarga el artefacto `XhampRust-windows-x64` al final de la pagina.
4. Extrae `XhampRust-windows-x64.zip` y ejecuta `XhampRust.exe`.
5. Se abrira `http://127.0.0.1:8765` en el navegador. Deja abierta la ventana del programa mientras uses el panel.

El ZIP contiene el ejecutable y este README. El proyecto y los datos de XAMPP no se incluyen en el ZIP. No hace falta un instalador: la aplicacion portable no escribe dentro de la carpeta de XAMPP.

## Requisitos de desarrollo y compilacion local

- Windows 10/11 x64.
- Rust estable (`rustup` y `cargo`).
- Visual Studio Build Tools con **Desktop development with C++** y el Windows SDK para el destino MSVC.
- XAMPP instalado en `C:\xampp`. Para otra ubicacion, configura `XAMPP_HOME`.

Abre PowerShell en la carpeta del proyecto:

```powershell
cargo fmt --all -- --check
cargo test --locked
cargo build --release --locked
```

El ejecutable queda en `target\release\xhamp-rust.exe`. Puedes ejecutarlo directamente o copiarlo a una carpeta portable y renombrarlo `XhampRust.exe`. El primer inicio crea los proyectos y las descargas en `%LOCALAPPDATA%\XhampRust` y abre el panel en el navegador.

### Compilar usando GitHub Codespaces

Codespaces sirve para editar el codigo, hacer commits y ejecutar `cargo fmt` y `cargo test`. El entorno Codespaces es Linux, por eso su ejecutable **no es el `.exe` de Windows**. Para obtener el `.exe` sin instalar Visual Studio localmente, usa **Actions > Build Windows portable app > Run workflow** y descarga el ZIP generado por Windows.

El workflow tambien se ejecuta en cada push a `main` y en etiquetas `v*`; sus paquetes aparecen como artefactos de la ejecucion. Los artefactos de Actions se conservan durante 30 dias.

## Funciones

- Estado y controles de Apache y MariaDB/MySQL de XAMPP.
- Apagado normal; el panel no termina procesos que no inicio.
- Creador de sitios PHP con una pagina inicial valida y protegida contra inyeccion HTML.
- Servidor PHP limitado a `127.0.0.1` y enlace a phpMyAdmin de XAMPP (`/phpmyadmin/`).
- Catalogo oficial, descarga de PHP Thread Safe x64 y MariaDB x64, y comprobacion SHA-256.
- Versiones de PHP aisladas y seleccionables sin reemplazar XAMPP.

Las versiones se guardan en `%LOCALAPPDATA%\XhampRust`. El panel no elimina versiones ni modifica la carpeta o los datos de XAMPP. MariaDB se descarga y extrae, pero no se activa sobre la base existente: cambiar la version de una base de datos requiere una migracion y una copia de seguridad explicitas.

El catalogo requiere internet. Los logs quedan en `%LOCALAPPDATA%\XhampRust\logs`. Apache suele usar el puerto 80, MariaDB el 3306 y el panel el 8765. Si un puerto esta ocupado, se informa el conflicto sin cerrar el proceso que lo ocupa. El enlace a phpMyAdmin requiere Apache iniciado.

La configuracion de correo de XAMPP/Mercury queda fuera de esta primera version.

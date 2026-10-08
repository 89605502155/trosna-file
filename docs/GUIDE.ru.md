# Руководство: как создать и использовать файл `.trosna`

[English version](GUIDE.md) · [README](../README.ru.md) ·
[Демонстрационный репозиторий](https://github.com/89605502155/trosna-file-demo)

Файл `.trosna` хранит один или несколько временных рядов. Каждый ряд
принадлежит *устройству* — таблице, в которой у каждой строки есть метка
времени и значения столбцов (например, `temperature`, `humidity`). Создать
такой файл можно тремя способами: из командной строки (например, из CSV), из
Python и из программы на Rust. Ниже каждый способ разобран по шагам.

## 1. Установка

Нужен Rust 1.89 или новее (установка: <https://rustup.rs>). Для Python нужен
Python 3.11 или новее.

| Что нужно | Команда |
|---|---|
| Утилита `trosna` | `cargo install --git https://github.com/89605502155/trosna-file trosna-cli` |
| Библиотека для Rust | в `Cargo.toml`: `trosna = { git = "https://github.com/89605502155/trosna-file" }` |
| Пакет для Python | `pip install "trosna @ git+https://github.com/89605502155/trosna-file#subdirectory=python"` |
| pandas и Polars (по желанию) | `pip install pandas polars` |

Пакет для Python собирается из исходного кода, поэтому Rust тоже должен быть
установлен. Проверка установки: `trosna --version`,
`python -c "import trosna; print(trosna.EXTENSION)"`.

## 2. Первый файл за минуту

### Из CSV (командная строка)

Возьмём файл `room.csv`:

```csv
time,temperature,humidity
2026-10-08T10:00:00+03:00,21.5,40
2026-10-08T10:00:10+03:00,21.6,41
2026-10-08T10:00:20+03:00,21.7,41
```

```sh
trosna convert room.csv room.trosna --device room1
trosna info room.trosna          # устройства, столбцы, число точек, размер
trosna cat room.trosna           # данные обратно в виде CSV
```

Типы столбцов определяются автоматически (целые → `int64`, дробные →
`float64`, `true/false` → `bool`, остальное → `string`; пустая ячейка —
пропуск). Первый столбец считается временем; другой можно указать через
`--time ИМЯ`. Время в формате ISO 8601 со смещением задаёт часовой пояс
устройства; целые числа понимаются как время от начала эпохи Unix, а единица
(`s`, `ms`, `us`, `ns`) угадывается по величине или задаётся `--unit`.

### Из Python

```python
import pandas as pd
import trosna

df = pd.DataFrame({
    "time": pd.date_range("2026-10-08 10:00", periods=5, freq="10s", tz="Europe/Moscow"),
    "temperature": [21.5, 21.6, 21.7, 21.6, 21.8],
})
trosna.write("room.trosna", df, device="room1")   # создаёт файл (или заменяет)
print(trosna.read_pandas("room.trosna"))
```

Вместо pandas можно передать `polars.DataFrame` или `pyarrow.Table`. Столбец
времени ищется автоматически (первый столбец с типом «метка времени» или
столбец с именем `time`, `timestamp`, …); его можно указать явно:
`time_column="ts"`. Если время записано целыми числами, нужна единица:
`unit="ms"`.

### Из Rust

```rust
use trosna::{Batch, Column, CommitOptions, DataType, DeviceSchema, TimeUnit, Writer};

fn main() -> trosna::Result<()> {
    let mut writer = Writer::create("room.trosna")?;
    writer.create_device(
        DeviceSchema::new("room1", TimeUnit::Second)
            .with_timezone("Europe/Moscow")
            .with_column("temperature", DataType::Float64),
    )?;
    let t0 = 1_791_442_800; // 2026-10-08 10:00:00 по Москве, секунды Unix
    let time: Vec<i64> = (0..5).map(|i| t0 + 10 * i).collect();
    let values = vec![21.5, 21.6, 21.7, 21.6, 21.8];
    writer.write("room1", &Batch::new(time, vec![("temperature", Column::from(values))])?)?;
    writer.commit_with(CommitOptions::message("первые измерения"))?;
    writer.close()
}
```

Время в библиотеке Rust — целое число единиц (`TimeUnit`) от 1970-01-01 UTC.

## 3. Что важно знать

* **Коммиты.** Все изменения попадают в файл атомарными коммитами. В Python
  каждый вызов `trosna.write` и каждый блок `with f.edit(...)` — один коммит; в
  Rust коммит делает `commit_with`, а `close` фиксирует оставшееся. Если
  программа упала до коммита, файл читается в состоянии последнего коммита,
  а незавершённый хвост удалит следующая запись или `trosna recover`.
* **Повтор метки времени** в одном устройстве заменяет точку (побеждает
  последняя запись); точки при чтении всегда упорядочены по времени.
* **Время без смещения UTC** — это показание часов в часовом поясе
  устройства. В повторяющемся при переводе часов часе берётся более ранний
  момент, а несуществующее время (пропущенный час) — ошибка.
* **Точность.** Время точки должно точно выражаться в единице устройства
  (нельзя записать `10:00:00.5` в устройство с единицей `s`). Границы
  диапазона при чтении округляются внутрь диапазона.
* **Несколько устройств.** В файле может быть сколько угодно устройств; при
  чтении укажите нужное: `trosna cat f.trosna --device room2`,
  `trosna.read_pandas("f.trosna", "room2")`.

## 4. Дозапись и правка данных

```sh
trosna convert more.csv room.trosna --append --device room1        # дописать CSV
trosna insert room.trosna --time "2026-10-08 10:01:00" temperature=22.0 -m "ручной ввод"
trosna update room.trosna --time "2026-10-08 10:00:20" temperature=21.65 -m "поверка датчика"
trosna delete room.trosna --time "2026-10-08 10:00:10"
trosna delete room.trosna --from "2026-10-08 10:00:30" --to "2026-10-08 10:00:40"
```

```python
f = trosna.open("room.trosna")
f.write(more_df, "room1")                               # дописать (новый коммит)
with f.edit(message="поверка датчика", author="Ферубко") as tx:
    tx.update("room1", "2026-10-08 10:00:20", temperature=21.65)
    tx.delete("room1", "2026-10-08 10:00:10")
    tx.insert("room1", "2026-10-08 10:01:00", temperature=22.0)
```

Если внутри блока `with` возникло исключение, ни одно из изменений не
записывается.

## 5. Метки (аннотации) интервалов

Метка — подписанный интервал времени с необязательным комментарием, например
«дверь открыта» или «калибровка».

```sh
trosna annotate room.trosna --start "2026-10-08 10:00:15" --end "2026-10-08 10:00:25" \
    --label "дверь открыта" --note "проветривание"
trosna annotations room.trosna
trosna annotate room.trosna --id 1 --label "окно открыто"    # изменить
trosna unannotate room.trosna 1                               # удалить
```

```python
with f.edit() as tx:
    tx.annotate("room1", "2026-10-08 10:00:15", "2026-10-08 10:00:25", "дверь открыта")
print(f.annotations())
```

## 6. История и «путешествие во времени»

```sh
trosna log room.trosna                     # список коммитов с хэшами SHA-256
trosna cat room.trosna --as-of 1           # данные после коммита 1
trosna diff room.trosna --from 1           # что изменилось после коммита 1
trosna diff room.trosna --from 2 --to 3
```

```python
f.commits()                 # история
f.read_pandas(as_of=1)      # прежняя версия
f.diff(1).points            # добавленные, удалённые и изменённые точки
```

Каждый коммит хранит хэш предыдущего, поэтому незаметно изменить историю
нельзя: `trosna verify` проверит контрольные суммы всех кадров и всю цепочку.

## 7. Чтение части данных

```sh
trosna cat room.trosna --columns temperature --from "2026-10-08 10:00:10" --to "2026-10-08 10:00:30"
trosna cat room.trosna --format table --limit 20
trosna convert room.trosna room.csv        # выгрузить в CSV
```

```python
trosna.read_arrow("room.trosna", columns=["temperature"], start="2026-10-08 10:00:10")
trosna.read_polars("room.trosna")
for batch in trosna.read_batches("room.trosna"):   # потоком, по блокам
    process(batch)
```

## 8. Обслуживание

| Задача | Командная строка | Python |
|---|---|---|
| Проверить целостность | `trosna verify f.trosna` | `trosna.verify("f.trosna")` |
| Восстановить после сбоя | `trosna recover f.trosna` | `trosna.recover("f.trosna")` |
| Уплотнить (убрать историю) | `trosna compact f.trosna new.trosna` | `trosna.compact("f.trosna", "new.trosna")` |

Уплотнённый файл содержит только текущие данные и метки, но его первый коммит
ссылается на хэш исходной версии, так что происхождение данных сохраняется.

## 9. Частые вопросы

* **«already exists»** — `convert` не перезаписывает файлы молча: добавьте
  `--append`, чтобы дописать, или `--force`, чтобы заменить.
* **«the file has several devices»** — укажите `--device` (или `device=` в Python).
* **«is more precise than the device's time unit»** — время точки содержит
  доли, которых нет в единице устройства; выберите более мелкую единицу при
  создании (`--unit ms`) или округлите время.
* **«does not exist in time zone»** — указано время, пропущенное при переходе
  на летнее время; укажите время со смещением UTC.
* **Где спецификация формата?** — [SPEC.md](SPEC.md); обоснование решений —
  [DESIGN.md](DESIGN.md).

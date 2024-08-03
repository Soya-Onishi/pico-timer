#![no_std]
#![no_main]

use defmt::*;
use defmt_rtt as _;
use panic_probe as _;

// rp_picoクレートをBSPとして使用する
use rp_pico as bsp;

// rp2040_pacをPAC（Peripheral Access Crate）として使用する
use bsp::hal::pac;
use bsp::hal::{clocks::init_clocks_and_plls, gpio, sio::Sio, timer, watchdog};
use bsp::{entry, hal::timer::Alarm};

use pac::interrupt;

// ピンの出力トグルメソッドを使用するために必要。
// StatefulOutputPinトレイトで宣言されているtoggleメソッドを使うため。
use embedded_hal::digital::StatefulOutputPin;

use core::cell::{Cell, RefCell};
use core::ops::DerefMut;
use cortex_m::interrupt::{free, CriticalSection, Mutex};

// これで100.micros()みたいに整数から時間を表す数値へ変換ができるようになる
// u32にトレイトを追加して型の機能を拡張したイメージ
use fugit::ExtU32;

// 冗長な型定義をやめるためにtypeで型を定義
// C言語のtypedefのようなもの。
// ただし、ジェネリクスが使えるので柔軟性はこっちのほうが高い。
type GlobalPeripheral<T> = Mutex<RefCell<Option<T>>>;
// constfnは同じ引数の値に対して必ず同じ結果を返す関数
// 引数なしなら必ず同じ値を返す。
// ただし、ジェネリクスは使えるので型の異なる値を返すことはできる。
const fn initial_global_peripheral<T>() -> GlobalPeripheral<T> {
    Mutex::new(RefCell::new(None))
}

// constfnでグローバル変数の初期値を設定。
// 必ず同じ結果になるのでコンパイル時点で式の評価を行い、結果をグローバル変数の初期値としている。
static ALARM0: GlobalPeripheral<timer::Alarm0> = initial_global_peripheral();
static LED: GlobalPeripheral<
    gpio::Pin<gpio::bank0::Gpio25, gpio::FunctionSioOutput, gpio::PullDown>,
> = initial_global_peripheral();

static INTERRUPT_COUNTER: Mutex<Cell<u32>> = Mutex::new(Cell::new(0));

const ALARM0_INTERVAL_MS: u32 = 1000;

#[entry]
fn main() -> ! {
    // ペリフェラルがまとめて入っている構造体を取得します。
    // ペリフェラルが構造体に入れることで、
    // ペリフェラルを触る際にもRustの所有権機能を利用することになります。
    //
    // Rustでは基本的にメモリアドレスを直接触る
    // unsafeなコードは書かないようにという考え方があります。
    // そういうコードは基本的にHALクレートの中に閉じ込められ、
    // アプリケーション部分の開発者はHALクレートが提供する
    // ペリフェラルの構造体およびTraitを利用することになります
    let mut pac = pac::Peripherals::take().unwrap();

    // Single Cycle IO
    // 1サイクルでアクセス可能なI/Oポート。
    // クレートの説明に`Provides core-local and inter-core hardware for the two processors, with single-cycle access.`とあるので、
    // アクセス競合が起こらないI/Oポートのことを言っているのでは
    //
    // ARM公式ドキュメント：https://developer.arm.com/documentation/dui0662/b/Cortex-M0--Peripherals/Single-cycle-I-O-Port
    // rp2040-pac SIO: https://docs.rs/rp2040-pac/latest/rp2040_pac/struct.SIO.html
    let sio = Sio::new(pac.SIO);

    // init_clocks_and_plls()で必要になるので、
    // watchdogをここでインスタンス化しておきます。
    // watchdog自体は有効になっていません。
    let mut watchdog = watchdog::Watchdog::new(pac.WATCHDOG);

    // クロック関連の設定を初期化
    let clocks = init_clocks_and_plls(
        bsp::XOSC_CRYSTAL_FREQ,
        pac.XOSC,
        pac.CLOCKS,
        pac.PLL_SYS,
        pac.PLL_USB,
        &mut pac.RESETS,
        &mut watchdog,
    )
    .ok()
    .unwrap();

    // Watchdogを開始。
    // WDに供給するクロックなどの設定は上のinit_clocks_and_plls()で済ませているので
    // ここではWDリセットまでの時間を設定すればよい。
    // watchdog.start(1_050_000.micros());
    //
    // WDをリスタートするときはfeed()を使う
    // watchdog.feed();

    // ピンを扱うインスタンスの作成
    let pins = bsp::Pins::new(
        pac.IO_BANK0,
        pac.PADS_BANK0,
        sio.gpio_bank0,
        &mut pac.RESETS,
    );

    // pins.ledでLEDにつながっているピンを指定する
    // rp-picoではGPIO25のピンにLEDがつながっているため、このような書き方をするよう。
    //
    // ちなみにピン定義にはマクロが使われているので、
    // パッと見でどういう定義になっているのかわかりにくい。
    let led_pin = pins.led.into_push_pull_output();

    // タイマー割り込み用のALARMを取り出す。
    let mut timer = timer::Timer::new(pac.TIMER, &mut pac.RESETS, &clocks);

    // alarm_0()は戻り値にOption<T>を使っている。
    // Option<T>は値を持っているかどうかわからないという変数。
    // 値が入っているかどうか（SomeかNoneか）を判定して、
    // 値がない＝どこかですでに使われていることを確認した上で処理を進めることもできる。
    //
    // 初めて取り出す場合は値が入っているのでここではunwrap()で強制的に値を取り出している。
    let mut alarm0 = timer.alarm_0().unwrap();

    // スレッド間でデータ競合が起こらないようにしている
    // free関数はCritialSectionを渡すラムダを要求する。
    // このCriticalSectionのインスタンスをグローバル変数を参照、操作するときに使用する。
    //
    // グローバル変数はMutexになっていてそのままでは何もできない。
    // borrowメソッドをCriticalSectionととともに呼び出すことで、
    // グローバル変数への参照を手に入れることができる（RefCell）。
    // RefCellには操作のためのメソッドなどが用意されているので、それを利用する。
    free(|cs| {
        alarm0.enable_interrupt();
        alarm0.clear_interrupt();
        alarm0.schedule(ALARM0_INTERVAL_MS.micros()).unwrap();

        // CriticalSectionを使ってMutexの中身を操作している部分
        ALARM0.borrow(cs).replace(Some(alarm0));
        LED.borrow(cs).replace(Some(led_pin));
    });

    info!("Program start");

    unsafe {
        pac::NVIC::unmask(pac::Interrupt::TIMER_IRQ_0);
    }

    // free()は値を返すこともできます。
    // ※ジェネリクスの機能で同じ関数でも異なる戻り値の型を扱うことができる
    let get_interrupt_count = || free(|cs| INTERRUPT_COUNTER.borrow(cs).get());
    let mut counter_old = get_interrupt_count();
    loop {
        let interrupt_count = get_interrupt_count();
        if counter_old != interrupt_count {
            // C言語のprintfに相当するprintln!なども一例だが、Rustでは可変長引数というものが存在しない。
            // これも可変長引数自体がunsafeな存在であるためというのがある（はず）。
            // その代わり、可変長引数をマクロを使って再現するという方法をとっている。
            // 関数名のあとに「!」がつくものは関数型マクロというマクロの一種。
            // C言語のマクロとイメージとしては近いかも。
            //
            // ※printfなどの実装をしたことがある人ならunsafeだというのはなんとなくわかるはず。
            // ※可変長引数は関数の呼び出し元が与えた情報（printfならフォーマット文字列）を「信頼して」処理をすすめている。
            // ※そして、その与えられた情報が間違いの場合メモリ破壊などを起こす危険性がある。
            // ※だからGCCやClangではprintfのフォーマット文に引数の型と合わない指定子の記述があったりすると警告がでる。
            info!(
                "interrupt count incremented! {} - {}",
                counter_old, interrupt_count
            );
            counter_old = interrupt_count;
        }
    }
}

// #pragma interruptみたいなもの
// ただし、pragmaディレクティブのように処理系に紐付いたものではなく
// 属性マクロと呼ばれるマクロの一種。
// これをつけることで、コンパイル時にASTの操作が行われる（はず）。
#[interrupt]
fn TIMER_IRQ_0() {
    // TIMER_IRQ_0は多重割り込みが発生しないので
    // free()を使って割り込み禁止する必要がない。
    // そのため、unsafeではあるが、
    // CriticalSectionのトークンを得る関数を利用して、
    // 割り込み禁止の処理を省略する。
    let cs = unsafe { CriticalSection::new() };

    let mut alarm0 = ALARM0.borrow(&cs).borrow_mut();
    let mut led = LED.borrow(&cs).borrow_mut();

    // Copyトレイトが実装されている型はRefCellの変わりにCellが使える。
    // 生値を取り出すことができるため、とりだしたあとは書き換えでも何でもできる。
    // ※Copyトレイトが実装されている型のみなのはCellのgetメソッドにCopyのトレイト制約があるから。
    // ※つまり、Copyトレイトを実装した型でしかgetメソッドは使えなくなっている。
    //
    // ※LEDなどのペリフェラル用structにはCopyトレイトは実装されていないので、このメソッドは使えない。
    // ※Copyトレイトが実装されている=実行時にペリフェラルが複製される=ハードのクローンが物理的に湧いてでるなので
    // ※Copyトレイトが実装されていないのはイメージ的にも正しい。
    //
    // ※ちなみにRustの制約として、
    // ※すでに別ライブラリ（標準ライブラリ含む）で定義されているstructとトレイトを使って
    // ※新しくトレイトの実装をすることはできなくなっている。
    // ※今回の場合だと、LED用のペリフェラルの型（rp2040-pacライブラリ）に
    // ※無理やりCopyトレイト（coreライブラリ）を実装して
    // ※getメソッドを使えるようにしてやる！みたいなことはできず、コンパイルエラーになる。
    let counter = INTERRUPT_COUNTER.borrow(&cs).get();
    if let (Some(alarm0), Some(led)) = (alarm0.deref_mut(), led.deref_mut()) {
        alarm0.clear_interrupt();
        alarm0.schedule(ALARM0_INTERVAL_MS.micros()).unwrap();

        led.toggle().unwrap();
    }

    INTERRUPT_COUNTER.borrow(&cs).set(counter.wrapping_add(1));
}

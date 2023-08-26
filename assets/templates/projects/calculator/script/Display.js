class Display {
    constructor(calculatorElement) {
        this.displayElement = calculatorElement.querySelector('.display');
    }

    update(value) {
        this.displayElement.textContent = value || '0';
    }

    reset() {
        this.displayElement.textContent = '0';
    }
}

export default Display;